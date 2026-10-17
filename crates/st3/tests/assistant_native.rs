//! Isolated real router/store coverage; no daemon, subprocess, ambient profile, or managed agent.
use std::{path::PathBuf, sync::Arc};

use axum::{Router, body::Body, http::{Request, StatusCode}};
use http_body_util::BodyExt as _;
use serde::Serialize;
use serde_json::{Value, json};
use st3::{api::AppState, model::{ClaimInput, PersonAskRequest}, store::Store};
use st3_client::{ActionRequest, AssistantGrant, AssistantGrantPolicy, AssistantMinimalReadMode,
    AssistantPairedSession, AssistantPairingBegin, AssistantPrincipal, AssistantScope,
    Fence, MessageKind, MessageSendParameters, PairingComplete, PersonAnswerInput,
    PersonStepParameters};
use tower::ServiceExt as _;

const OWNER: &str = "person/ada";
const AGENT: &str = "agent/native.worker";
const POLICY: &str = "custom.client.assistant.v1.policy";

struct Harness {
    root: tempfile::TempDir,
    state: AppState,
    local: Router,
    fabric: Router,
}

impl Harness {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(&root.path().join("store.sqlite"), "assistant-native").unwrap());
        store.bind_fleet("3c9a1f2e-8b7d-4e6c-a5f4-1d2e3c4b5a69").unwrap();
        store.set_node_key(Arc::new(smallclaims::fleet::MemberKey::generate().unwrap().0)).unwrap();
        let source = format!("version 2\nagent \"native.worker\" {{ workspace {:?}; command \"true\"; restart always; }}\nmission \"native-mission\" state=\"ready\" {{ goal \"Native integration mission\"; }}", root.path());
        store.apply_internal(&st3::graph::parse_intent(&source, "assistant-native").unwrap(), "native-seed").unwrap();
        let state = AppState {
            owning_person: Some(OWNER.into()), store,
            notify: Arc::new(tokio::sync::Notify::new()),
            event_notify: tokio::sync::watch::channel(0).0,
            node: "assistant-native".into(), state_dir: root.path().into(),
            pty_root: root.path().join("pty"), pty_binary: PathBuf::from("pty"),
            fleet_id: None, configured_peers: vec![], client_relay: None,
            native_session_home: None, planner_default: Default::default(),
        };
        Self { local: st3::api::router(state.clone()), fabric: st3::api::fabric_router(state.clone()), root, state }
    }

    async fn owner(&self, method: &str, path: &str, body: &impl Serialize) -> Value {
        let (status, value) = request(&self.local, method, path, None, Some(OWNER), body).await;
        assert_eq!(status, StatusCode::OK, "{path}: {value}");
        value
    }

    async fn assistant(&self, paired: &AssistantPairedSession, method: &str, path: &str, body: &impl Serialize) -> (StatusCode, Value) {
        request(&self.fabric, method, path, Some(&paired.credential), None, body).await
    }

    async fn pair(&self) -> AssistantPairedSession {
        let begin = AssistantPairingBegin {
            api_version: st3_client::API_VERSION.into(), device_name: "Native ChatGPT test".into(),
            assistant_id: AssistantPrincipal::Chatgpt, scopes: scopes(), expires_in_seconds: Some(3600),
            disclosure_consent: true, consent_evidence: "Isolated integration disclosure consent".into(),
            minimal_read_mode: Some(AssistantMinimalReadMode::CallerBoundAllow),
        };
        let challenge = self.owner("POST", "/v1/client/pairings", &begin).await;
        let path = format!("/v1/client/pairings/{}/complete", challenge["value"]["pairing_id"].as_str().unwrap().trim_start_matches("pairing/"));
        let complete = PairingComplete {
            api_version: st3_client::API_VERSION.into(), code: challenge["value"]["code"].as_str().unwrap().into(),
            device_public_key: "BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc".into(),
            key_storage: Some("software".into()),
        };
        let (status, value) = request(&self.fabric, "POST", &path, None, None, &complete).await;
        assert_eq!(status, StatusCode::OK, "{value}");
        let paired: AssistantPairedSession = serde_json::from_value(value["value"].clone()).unwrap();
        assert_eq!(paired.grant.issuer, OWNER);
        assert_eq!(paired.principal, AssistantPrincipal::Chatgpt);
        assert_eq!(paired.grant.minimal_read_mode, AssistantMinimalReadMode::CallerBoundAllow);
        let (status, summary) = self.assistant(&paired, "GET", "/v1/client/assistant/summary", &()).await;
        assert_eq!(status, StatusCode::OK, "fresh pairing must read without a policy call: {summary}");
        for id in [paired.grant.id.as_str(), paired.grant.id.trim_start_matches("assistant-grant/")] {
            let inspected = self.owner("GET", &format!("/v1/client/assistant/grants/{}", urlencoding::encode(id)), &()).await;
            assert_eq!(inspected["value"]["id"], paired.grant.id);
            let raw = self.owner("GET", &format!("/v1/client/assistant/grants/{id}"), &()).await;
            assert_eq!(raw["value"]["id"], paired.grant.id);
        }
        let (replay, _) = request(&self.fabric, "POST", &path, None, None, &complete).await;
        assert_eq!(replay, StatusCode::FORBIDDEN);
        self.policy(&paired, scopes()).await;
        paired
    }

    async fn policy(&self, paired: &AssistantPairedSession, scopes: Vec<AssistantScope>) -> AssistantGrant {
        let policy = AssistantGrantPolicy {
            api_version: st3_client::API_VERSION.into(), scopes, expires_in_seconds: Some(3600),
            disclosure_consent: true, consent_evidence: "Native owner explicit policy".into(),
            minimal_read_mode: AssistantMinimalReadMode::CallerBoundAllow,
            recipient_allowlist: Some(vec![AGENT.into()]),
        };
        let value = self.owner("POST", &grant_path(paired, "/policy"), &policy).await;
        serde_json::from_value(value["value"].clone()).unwrap()
    }

    fn ask(&self, key: &str, title: &str) -> String {
        self.state.store.ask_person(&PersonAskRequest {
            legacy_request: None, person: OWNER.into(), title: title.into(), reason: "Owner must choose".into(),
            actor: AGENT.into(), step: None, new_run: Some(key.into()), incarnation: None,
            request: Some(json!({"version":1,"type":"choice","question":"Which option?","why_person":"Owner chooses.","answers":[{"id":"yes","label":"Yes","consequence":"Record yes."},{"id":"no","label":"No","consequence":"Record no."}]})),
            idempotency_key: key.into(),
        }).unwrap().subject
    }

    async fn fence(&self) -> Fence {
        let value = self.owner("GET", "/v1/client/capabilities", &()).await;
        Fence { snapshot_id: value["snapshot"]["id"].as_str().unwrap().into(), ..Default::default() }
    }

    async fn answer(&self, target: &str, answer: &str, key: &str) -> ActionRequest {
        let item = self.state.store.attention_items(Some(OWNER)).unwrap().into_iter().find(|item| item.subject == target).unwrap();
        ActionRequest::work_done(format!("action/{key}"), format!("native-test-{key}"), self.fence().await,
            PersonStepParameters { target_id: target.into(), summary: "Native test answer".into(), episode: item.episode,
                answer: Some(PersonAnswerInput { id: Some(answer.into()), ..Default::default() }), ..Default::default() }).unwrap()
    }

    async fn message(&self, to: &str, key: &str) -> ActionRequest {
        ActionRequest::message_send(format!("action/{key}"), format!("native-test-{key}"), self.fence().await,
            MessageSendParameters { to: to.into(), content: "Native assistant message".into(), title: None,
                in_reply_to: None, session_id: None, tags: vec![], kind: MessageKind::Wake,
                attachments: vec![], signature: None }).unwrap()
    }

    fn expire(&self, paired: &AssistantPairedSession) {
        let subject = grant_subject(paired);
        let previous = self.state.store.claims_for_subject_kind_at(&subject, POLICY, None, true, 1).unwrap().claims.pop().unwrap();
        let mut fields = previous.body["fields"].clone();
        fields["expires_at_unix_ms"] = json!(1);
        self.state.store.append_claim(&ClaimInput { subject, kind: POLICY.into(), actor: Some(OWNER.into()),
            fields: serde_json::from_value(fields).unwrap(), evidence: vec![previous.id.clone()],
            expected_subject: Some(Some(previous.id)), idempotency_key: None }).unwrap();
    }
}

async fn request(app: &Router, method: &str, path: &str, bearer: Option<&str>, person: Option<&str>, body: &impl Serialize) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(path).header("content-type", "application/json");
    if let Some(bearer) = bearer { builder = builder.header("authorization", format!("Bearer {bearer}")); }
    if let Some(person) = person { builder = builder.header("x-st3-person", person); }
    let response = app.clone().oneshot(builder.body(Body::from(serde_json::to_vec(body).unwrap())).unwrap()).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}

fn scopes() -> Vec<AssistantScope> {
    vec![AssistantScope::Summary, AssistantScope::Attention, AssistantScope::Missions,
        AssistantScope::Agents, AssistantScope::Conversation, AssistantScope::Messages]
}
fn grant_path(paired: &AssistantPairedSession, suffix: &str) -> String {
    format!("/v1/client/assistant/grants/{}{suffix}", paired.grant.id.trim_start_matches("assistant-grant/"))
}
fn grant_subject(paired: &AssistantPairedSession) -> String {
    format!("custom/client/assistant-v1-{}", paired.grant.id.trim_start_matches("assistant-grant/"))
}

#[tokio::test]
async fn pairing_caller_bound_rich_reads_and_budget_do_not_silently_truncate() {
    let h = Harness::new();
    let ask = h.ask("rich-ask", "Owner-visible rich question");
    let paired = h.pair().await;
    for projection in ["summary", "attention-rich", "missions-rich", "agents-rich"] {
        let (status, value) = h.assistant(&paired, "GET", &format!("/v1/client/assistant/{projection}?max_response_bytes=16384"), &()).await;
        assert_eq!(status, StatusCode::OK, "{projection}: {value}");
        assert!(value["value"].is_object());
        if projection == "attention-rich" { assert!(value.to_string().contains("Owner-visible rich question")); }
        if projection == "missions-rich" { assert!(value["value"]["items"].as_array().unwrap().iter().any(|item| item["title"] == "native-mission")); }
        if projection == "agents-rich" { assert!(value.to_string().contains(AGENT)); }
    }
    let path = format!("/v1/client/assistant/attention-content?alert_id={}&max_response_bytes=16384", urlencoding::encode(&ask));
    let (status, value) = h.assistant(&paired, "GET", &path, &()).await;
    assert_eq!(status, StatusCode::OK, "{value}");
    assert_eq!(value["value"]["request"]["question"], "Which option?");
    let (status, value) = h.assistant(&paired, "GET", "/v1/client/assistant/attention-rich?max_response_bytes=1", &()).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{value}");
    assert_eq!(value["code"], "response-budget-exceeded");
    assert!(value.get("value").is_none(), "A failed bounded read must not masquerade as a complete page");
}

#[tokio::test]
async fn typed_owner_approval_executes_a_durable_answer_and_stale_episode_is_rejected() {
    let h = Harness::new();
    let ask = h.ask("answer-ask", "Answer through approval");
    let paired = h.pair().await;
    let answer = h.answer(&ask, "yes", "assistant-answer").await;
    let (status, pending) = h.assistant(&paired, "POST", "/v1/client/assistant/propose", &answer).await;
    assert_eq!(status, StatusCode::OK, "{pending}");
    assert_eq!(pending["value"]["status"], "approval-required");
    assert!(h.state.store.step_run(&ask).unwrap().unwrap().person_answers.is_empty());
    let approval = pending["value"]["approval_id"].as_str().unwrap();
    let approve = h.answer(approval, "approve", "owner-approve").await;
    h.owner("POST", "/v1/client/actions", &approve).await;
    let (status, result) = h.assistant(&paired, "POST", "/v1/client/assistant/propose", &answer).await;
    assert_eq!(status, StatusCode::OK, "{result}");
    let durable = h.state.store.step_run(&ask).unwrap().unwrap();
    assert_eq!(durable.person_answers.len(), 1);
    assert_eq!(durable.person_answers[0].answer.as_ref().unwrap()["id"], "yes");

    let stale_ask = h.ask("stale-ask", "Stale answer");
    let mut stale = serde_json::to_value(h.answer(&stale_ask, "yes", "stale-answer").await).unwrap();
    stale["parameters"]["episode"] = json!("not-the-current-episode");
    let stale: ActionRequest = serde_json::from_value(stale).unwrap();
    let (status, pending) = h.assistant(&paired, "POST", "/v1/client/assistant/propose", &stale).await;
    assert_eq!(status, StatusCode::OK, "{pending}");
    let approve = h.answer(pending["value"]["approval_id"].as_str().unwrap(), "approve", "approve-stale").await;
    h.owner("POST", "/v1/client/actions", &approve).await;
    let (status, denied) = h.assistant(&paired, "POST", "/v1/client/assistant/propose", &stale).await;
    assert!(!status.is_success(), "{denied}");
    assert_eq!(denied["code"], "stale-fence");
    assert!(h.state.store.step_run(&stale_ask).unwrap().unwrap().person_answers.is_empty());
}

#[tokio::test]
async fn allowlisted_message_is_idempotent_and_scope_recipient_revocation_expiry_fail_closed() {
    let h = Harness::new();
    let paired = h.pair().await;
    let message = h.message(AGENT, "allowlisted-message").await;
    let (status, first) = h.assistant(&paired, "POST", "/v1/client/assistant/propose", &message).await;
    assert_eq!(status, StatusCode::OK, "{first}");
    let (status, replay) = h.assistant(&paired, "POST", "/v1/client/assistant/propose", &message).await;
    assert_eq!(status, StatusCode::OK, "{replay}");
    assert_eq!(first["value"]["affected_ids"], replay["value"]["affected_ids"]);
    assert!(first["value"]["affected_ids"].as_array().unwrap().iter().any(|id| id.as_str().is_some_and(|id| id.starts_with("message/"))));
    assert_eq!(h.state.store.messages(Some(AGENT), true).unwrap().len(), 1);
    let subject = first["value"]["affected_ids"].as_array().unwrap().iter()
        .filter_map(Value::as_str).find(|id| id.starts_with("message/")).unwrap();
    let claim = h.state.store.latest_claim(subject, Some("message.sent")).unwrap().unwrap();
    assert_eq!(claim.actor.as_deref(), Some("assistant/chatgpt"));
    assert_eq!(claim.body["fields"]["from"], "assistant/chatgpt");
    assert_eq!(claim.body["fields"]["assistant_owner"], OWNER);
    assert_eq!(claim.body["fields"]["assistant_session_actor"], paired.session_actor);
    assert_eq!(claim.body["fields"]["sender_class"], "external-assistant");
    assert_eq!(claim.body["fields"]["assistant_grant_id"], paired.grant.id);
    let projected = h.state.store.messages(Some(AGENT), true).unwrap().pop().unwrap();
    assert_eq!(projected.from, "assistant/chatgpt");
    let metadata = projected.assistant_metadata().unwrap();
    let notification = st_drivers::ding::st3_notification_with_metadata(
        &projected.subject, &projected.from, &projected.to, projected.title.as_deref(),
        &projected.content, &"0".repeat(64),
        st_drivers::ding::NotificationMetadata { attachments: &[], assistant: Some(&metadata) },
    );
    assert!(notification.contains("from=\"assistant/chatgpt\""));
    assert!(notification.contains(&format!("assistant_grant_id=\"{}\"", paired.grant.id)));
    assert!(notification.contains(st_drivers::ding::EXTERNAL_ASSISTANT_WARNING));
    assert!(!notification.contains(st_drivers::ding::PERSON_REPLY_INSTRUCTION));
    let audit = h.state.store.claims_page(None, None, 0, None, false, 1000).unwrap().claims
        .into_iter().find(|claim| claim.kind == "custom.client.assistant.v1.audit"
            && claim.body["fields"]["operation"] == "grant-authorized-write").unwrap();
    assert_eq!(audit.actor.as_deref(), Some(paired.session_actor.as_str()));
    assert_eq!(audit.body["fields"]["actor"], paired.session_actor);
    assert_eq!(audit.body["fields"]["principal"], "assistant/chatgpt");
    assert_eq!(audit.body["fields"]["issuer"], OWNER);
    assert_eq!(audit.body["fields"]["grant_id"], paired.grant.id);
    let projected = serde_json::to_value(projected).unwrap();
    assert_eq!(projected["from"], "assistant/chatgpt");
    assert_eq!(projected["sender_class"], "external-assistant");
    assert_eq!(projected["assistant_grant_id"], paired.grant.id);
    let forbidden = h.message("agent/not-allowlisted", "recipient-denied").await;
    let (status, _) = h.assistant(&paired, "POST", "/v1/client/assistant/propose", &forbidden).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(h.state.store.messages(Some("agent/not-allowlisted"), true).unwrap().is_empty());
    h.policy(&paired, scopes().into_iter().filter(|scope| *scope != AssistantScope::Messages).collect()).await;
    let (status, _) = h.assistant(&paired, "POST", "/v1/client/assistant/propose", &message).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "Even a cached write must require the current scope");
    h.policy(&paired, scopes()).await;
    h.owner("POST", &grant_path(&paired, "/revoke"), &json!({})).await;
    let (status, _) = h.assistant(&paired, "GET", "/v1/client/assistant/agents-rich", &()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let expired = h.pair().await;
    h.expire(&expired);
    let (status, _) = h.assistant(&expired, "GET", "/v1/client/assistant/summary", &()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(h.state.store.messages(Some(AGENT), true).unwrap().len(), 1);
}

#[tokio::test]
async fn writer_transaction_rechecks_policy_revision_recipient_and_revocation() {
    use st3::store::assistant_write::{AssistantWriteAuthorization, authorize};
    let h = Harness::new();
    let paired = h.pair().await;
    let grant = h.policy(&paired, scopes()).await;
    let authorization = AssistantWriteAuthorization {
        grant_subject: grant_subject(&paired), actor: paired.session_actor.clone(), owner: OWNER.into(),
        policy_revision: grant.policy_revision, scope: "write.assistant.messages".into(),
        recipient: Some(AGENT.into()), approval_subject: None,
    };
    let mut connection = rusqlite::Connection::open(h.root.path().join("store.sqlite")).unwrap();
    let transaction = connection.transaction().unwrap();
    authorize(&transaction, "assistant-native", &authorization).unwrap();
    let mut wrong_recipient = authorization.clone();
    wrong_recipient.recipient = Some("agent/not-allowlisted".into());
    assert!(authorize(&transaction, "assistant-native", &wrong_recipient).is_err());
    let mut wrong_actor = authorization.clone();
    wrong_actor.actor = "assistant/chatgpt/session/another-key".into();
    assert!(authorize(&transaction, "assistant-native", &wrong_actor).is_err());
    assert!(authorize(&transaction, "another-member", &authorization).is_err());
    let mut missing_approval = authorization.clone();
    missing_approval.approval_subject = Some("step-run/nonexistent/ask".into());
    assert!(authorize(&transaction, "assistant-native", &missing_approval).is_err());
    transaction.rollback().unwrap();
    h.policy(&paired, scopes()).await;
    let transaction = connection.transaction().unwrap();
    assert!(authorize(&transaction, "assistant-native", &authorization).is_err(), "Authentication captured before a policy change cannot authorize the writer transaction");
    transaction.rollback().unwrap();
    let current = h.policy(&paired, scopes()).await;
    let current = AssistantWriteAuthorization { policy_revision: current.policy_revision, ..authorization };
    h.owner("POST", &grant_path(&paired, "/revoke"), &json!({})).await;
    let transaction = connection.transaction().unwrap();
    assert!(authorize(&transaction, "assistant-native", &current).is_err(), "Revocation must be visible at the mutation transaction, not just HTTP ingress");
    transaction.rollback().unwrap();
    let expired = h.pair().await;
    let grant = h.policy(&expired, scopes()).await;
    let expired_authorization = AssistantWriteAuthorization {
        grant_subject: grant_subject(&expired), actor: expired.session_actor.clone(), owner: OWNER.into(),
        policy_revision: grant.policy_revision, scope: "write.assistant.messages".into(),
        recipient: Some(AGENT.into()), approval_subject: None,
    };
    h.expire(&expired);
    let transaction = connection.transaction().unwrap();
    assert!(authorize(&transaction, "assistant-native", &expired_authorization).is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn synchronous_send_rechecks_revocation_and_recipient_removal_before_commit() {
    let h = Arc::new(Harness::new());
    let paired = h.pair().await;
    let gate = h.root.path().join("mutation-gate");
    st3::store::assistant_write_gate::install(&gate, Some(grant_subject(&paired))).unwrap();
    let connection = rusqlite::Connection::open(h.root.path().join("store.sqlite")).unwrap();
    let count_messages = || connection.query_row(
        "SELECT count(*) FROM claims WHERE kind='message.sent'", [], |row| row.get::<_, u64>(0),
    ).unwrap();
    for revoke in [false, true] {
        h.policy(&paired, scopes()).await;
        let before = count_messages();
        let request = h.message(AGENT, if revoke { "held-revoke" } else { "held-recipient" }).await;
        for name in ["hold", "entered", "release", "done"] {
            let path = gate.join(name);
            if path.exists() { std::fs::remove_file(path).unwrap(); }
        }
        std::fs::write(gate.join("hold"), b"hold").unwrap();
        let send_h = h.clone();
        let send_paired = paired.clone();
        let send = tokio::spawn(async move {
            send_h.assistant(&send_paired, "POST", "/v1/client/assistant/propose", &request).await
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !gate.join("entered").exists() {
            assert!(std::time::Instant::now() < deadline, "send did not reach the pre-commit gate");
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(!send.is_finished(), "synchronous send must still await its mutation");
        if revoke {
            h.owner("POST", &grant_path(&paired, "/revoke"), &json!({})).await;
        } else {
            h.owner("POST", &grant_path(&paired, "/policy"), &AssistantGrantPolicy {
                api_version: st3_client::API_VERSION.into(), scopes: scopes(),
                expires_in_seconds: Some(3600), disclosure_consent: true,
                consent_evidence: "Owner removes recipient during held send".into(),
                minimal_read_mode: AssistantMinimalReadMode::CallerBoundAllow,
                recipient_allowlist: Some(vec![]),
            }).await;
        }
        std::fs::remove_file(gate.join("hold")).unwrap();
        std::fs::write(gate.join("release"), b"release").unwrap();
        let (status, denial) = send.await.unwrap();
        assert!(!status.is_success(), "{denial}");
        assert_eq!(denial["code"], "forbidden", "{denial}");
        assert_eq!(count_messages(), before, "denied send must append no message claim");
    }
}

#[tokio::test]
async fn assistant_provenance_generic_and_native_forgery_is_refused() {
    let h = Harness::new();
    let paired = h.pair().await;
    for (index, (actor, extra)) in [
        ("assistant/chatgpt", json!({})),
        (OWNER, json!({"from":"assistant/chatgpt"})),
        (OWNER, json!({"sender_class":"external-assistant"})),
        (OWNER, json!({"assistant_grant_id":paired.grant.id})),
        (OWNER, json!({"assistant_owner":OWNER})),
        (OWNER, json!({"assistant_session_actor":paired.session_actor})),
    ].into_iter().enumerate() {
        let mut fields = json!({"from":OWNER,"to":AGENT,"content":"forged","status":"sent"});
        fields.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        let (status, error) = request(&h.local, "POST", "/v1/claims", None, Some(OWNER),
            &json!({"subject":format!("message/assistant-forged-{index}"),"kind":"message.sent","actor":actor,"fields":fields})).await;
        assert!(!status.is_success(), "{error}");
        assert_eq!(error["code"], "assistant-send-refused", "{error}");
    }
    let (status, _) = request(&h.local, "POST", "/v1/messages", None, Some(OWNER),
        &json!({"idempotency_key":"assistant-native-forged","from":"assistant/chatgpt","to":AGENT,"content":"forged"})).await;
    assert!(!status.is_success());
    for field in ["from", "actor", "sender_class", "assistant_grant_id", "assistant_owner", "assistant_session_actor"] {
        let mut message = serde_json::to_value(h.message(AGENT, field).await).unwrap();
        message["parameters"][field] = json!("forged");
        let (status, _) = h.assistant(&paired, "POST", "/v1/client/assistant/propose", &message).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }
    assert!(h.state.store.messages(Some(AGENT), true).unwrap().is_empty());
}
