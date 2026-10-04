//! Each test owns its store and Unix listener; no fleet daemon or seat is touched.
use super::*;

const SEAT: &str = "agent/garden/chooser";

struct Fixture {
    _root: tempfile::TempDir,
    state: AppState,
    client: crate::client::Client,
    server: tokio::task::JoinHandle<anyhow::Result<()>>,
    step: String,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl Fixture {
    async fn new(bound: bool) -> Self {
        fn chooser(_: u32) -> Option<String> {
            Some(SEAT.into())
        }
        fn unbound(_: u32) -> Option<String> {
            None
        }
        let root = tempfile::tempdir().unwrap();
        let state = super::tests::state(root.path());
        state
            .store
            .apply_internal(
                &parse_intent(
                    r#"version 2
agent "garden/chooser" { workspace "/tmp"; harness "claude" {} }
agent "garden/other" { workspace "/tmp"; harness "claude" {} }
mission "catalog" state="ready" {
  goal "Choose a format."
  step "choose" { assigned-to "agent/garden/chooser"; goal "Ask for a format." }
}
"#,
                    "node",
                )
                .unwrap(),
                "fixture",
            )
            .unwrap();
        let run = state
            .store
            .create_mission_run(&MissionRunRequest {
                mission: "catalog".into(),
                revision: None,
                workspace: "/tmp".into(),
                requester: Some("person/ada".into()),
                mode: Some("run".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "run".into(),
            })
            .unwrap();
        let step = run.steps[0].subject.clone();
        state.store.set_step_state(&step, "ready", None).unwrap();
        state
            .store
            .work_action(
                &step,
                "claim",
                &WorkRequest {
                    actor: Some(SEAT.into()),
                    incarnation: Some("chooser:one".into()),
                    summary: None,
                    reason: None,
                    evidence: vec![],
                    idempotency_key: "claim".into(),
                },
            )
            .unwrap();
        let socket = root.path().join("api.sock");
        let app = router(state.clone());
        let listener_socket = socket.clone();
        let server = tokio::spawn(async move {
            serve_unix_with_ancestor(
                &listener_socket,
                None,
                app,
                true,
                if bound { chooser } else { unbound },
            )
            .await
        });
        for _ in 0..100 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let fixture = Self {
            _root: root,
            state,
            client: crate::client::Client::unix(&socket),
            server,
            step,
        };
        fixture.runtime("chooser:one");
        fixture
    }

    fn runtime(&self, incarnation: &str) {
        self.state
            .store
            .append_claim(&ClaimInput {
                subject: SEAT.into(),
                kind: "runtime.observed".into(),
                actor: Some("daemon/runtime".into()),
                fields: BTreeMap::from([
                    ("status".into(), json!("running")),
                    ("incarnation_id".into(), json!(incarnation)),
                ]),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        self.state
            .store
            .append_claim(&ClaimInput {
                subject: SEAT.into(),
                kind: "harness.observed".into(),
                actor: Some(SEAT.into()),
                fields: BTreeMap::from([
                    ("state".into(), json!("working")),
                    ("driver".into(), json!("claude")),
                    ("incarnation_id".into(), json!(incarnation)),
                ]),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
    }

    fn ask(&self) -> Value {
        json!({"person":"person/ada", "title":"Choose a format", "reason":"The catalog needs a format.",
            "actor":SEAT, "step":self.step, "idempotency_key":"format",
            "request":{"version":1, "type":"choice", "question":"Choose a format.", "why_person":"Only the catalog owner can choose the format.",
                "answers":[{"id":"markdown", "label":"Markdown", "consequence":"Write Markdown."},{"id":"text", "label":"Text", "consequence":"Write plain text."}]}})
    }

    async fn refused(&self, body: Value, code: &str) -> String {
        let error = self
            .client
            .post::<_, Value>("/v1/work/ask", &body)
            .await
            .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains(code), "{message}");
        assert_eq!(
            self.state
                .store
                .step_run(&self.step)
                .unwrap()
                .unwrap()
                .status,
            "claimed"
        );
        message
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn claude_seat_asks_without_an_incarnation() {
    let fixture = Fixture::new(true).await;
    let ask: StepRunView = fixture
        .client
        .post("/v1/work/ask", &fixture.ask())
        .await
        .unwrap();
    assert_eq!(ask.assigned_to.as_deref(), Some("person/ada"));
    assert_eq!(
        fixture
            .state
            .store
            .step_run(&fixture.step)
            .unwrap()
            .unwrap()
            .status,
        "waiting-person"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn explicit_stale_ask_names_expected_and_given_incarnations() {
    let fixture = Fixture::new(true).await;
    let mut body = fixture.ask();
    body["incarnation"] = json!("chooser:stale");
    let message = fixture.refused(body, "stale-work-ask").await;
    assert!(message.contains("chooser:one"), "{message}");
    assert!(message.contains("chooser:stale"), "{message}");
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn restarted_seat_cannot_ask_with_its_old_incarnation() {
    let fixture = Fixture::new(true).await;
    fixture.runtime("chooser:two");
    // The previous claim has not yet been recovered: the runtime fence must still refuse it.
    let mut body = fixture.ask();
    body["incarnation"] = json!("chooser:one");
    let message = fixture.refused(body, "stale-work-ask").await;
    assert!(message.contains("chooser:two"), "{message}");
    assert!(message.contains("chooser:one"), "{message}");
}

#[tokio::test]
async fn unbound_request_cannot_fill_the_claim_incarnation() {
    let fixture = Fixture::new(false).await;
    fixture.refused(fixture.ask(), "stale-work-ask").await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn another_seat_cannot_borrow_the_bound_incarnation() {
    let fixture = Fixture::new(true).await;
    let mut body = fixture.ask();
    body["actor"] = json!("agent/garden/other");
    fixture.refused(body, "foreign-agent-actor").await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn bound_work_actions_and_extensions_also_fill_missing_incarnations() {
    let fixture = Fixture::new(true).await;
    let progress: StepRunView = fixture.client.post(
        &format!("/v1/work/progress/{}", fixture.step),
        &json!({"actor":SEAT, "summary":"Preparing the question.", "idempotency_key":"progress"}),
    ).await.unwrap();
    assert_eq!(progress.claim_incarnation.as_deref(), Some("chooser:one"));
    let extended: StepRunView = fixture.client.post(
        &format!("/v1/work/extend/{}", fixture.step),
        &json!({"actor":SEAT, "by_ms":60_000, "reason":"More time to prepare.", "idempotency_key":"extend"}),
    ).await.unwrap();
    assert_eq!(extended.claim_incarnation.as_deref(), Some("chooser:one"));
}

#[tokio::test]
async fn an_unbound_caller_can_still_supply_an_explicit_claim_fence() {
    let fixture = Fixture::new(false).await;
    let mut body = fixture.ask();
    body["incarnation"] = json!("chooser:one");
    let ask: StepRunView = fixture.client.post("/v1/work/ask", &body).await.unwrap();
    assert_eq!(ask.assigned_to.as_deref(), Some("person/ada"));
}
