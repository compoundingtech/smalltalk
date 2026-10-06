//! Publication controls use an isolated HTTP endpoint and the real graph's idempotency fold.
//! They never launch a provider or modify a live seat.
use super::*;
use axum::{
    Json, Router,
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use std::sync::{
    Mutex,
    atomic::{AtomicU8, Ordering},
};

struct Control {
    store: Store,
    owner: st3::mailbox::Fence,
    attachment: AtomicU8,  // 0: absent, 1: admitted, 2: failed check
    publication: AtomicU8, // 0: acknowledged, 1: no commit, 2: commit without acknowledgement
    requests: Mutex<Vec<Value>>,
}

async fn attachment(
    State(control): State<Arc<Control>>,
    Query(fence): Query<st3::mailbox::Fence>,
) -> Response {
    let mode = control.attachment.load(Ordering::SeqCst);
    if mode == 2 {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let owns =
        serde_json::to_value(&fence).unwrap() == serde_json::to_value(&control.owner).unwrap();
    Json(json!({"api_version":"st3.v1", "value":{"attached": mode == 1 && owns}})).into_response()
}

async fn publication(
    State(control): State<Arc<Control>>,
    Json(input): Json<ClaimInput>,
) -> Response {
    control
        .requests
        .lock()
        .unwrap()
        .push(serde_json::to_value(&input).unwrap());
    let mode = control.publication.swap(0, Ordering::SeqCst);
    if mode == 1 {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let record = control.store.append_claim(&input).unwrap();
    if mode == 2 {
        // Fail the HTTP body after commit: the caller cannot receive the operation's
        // acknowledgement, while the graph retains the canonical claim.
        return axum::body::Body::from_stream(futures_util::stream::once(async {
            Err::<Vec<u8>, _>(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                "lost acknowledgement",
            ))
        }))
        .into_response();
    }
    Json(json!({"api_version":"st3.v1", "value":record})).into_response()
}

struct Fixture {
    control: Arc<Control>,
    client: Client,
    mailbox: NativeMailbox,
    state: NativeLoopState,
    server: tokio::task::JoinHandle<()>,
}

impl Fixture {
    async fn new() -> Self {
        let store = Store::open_memory("attachment-publication-control").unwrap();
        let owner = st3::mailbox::Fence {
            subject: "agent/example/quartz".into(),
            incarnation: "7:quartz".into(),
            component: "title".into(),
            epoch: 5,
            token: "quartz-owner".into(),
        };
        store
            .append_claim(&ClaimInput {
                subject: owner.subject.clone(),
                kind: "runtime.observed".into(),
                actor: None,
                fields: BTreeMap::from([
                    ("status".into(), json!("running")),
                    ("incarnation_id".into(), json!(owner.incarnation)),
                ]),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        let control = Arc::new(Control {
            store,
            owner: owner.clone(),
            attachment: AtomicU8::new(1),
            publication: AtomicU8::new(0),
            requests: Mutex::new(vec![]),
        });
        let app = Router::new()
            .route("/v1/mailbox/attachment", get(attachment))
            .route("/v1/claims", post(publication))
            .with_state(control.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = Client::new(Endpoint::Http(format!(
            "http://{}",
            listener.local_addr().unwrap()
        )));
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            state: NativeLoopState {
                mailbox_fence: Some(owner.clone()),
                ..Default::default()
            },
            mailbox: NativeMailbox {
                subscription: None,
                fence: owner,
                messages: vec![],
                queued: BTreeMap::new(),
                replayed: false,
                last_title_warning: None,
            },
            control,
            client,
            server,
        }
    }

    async fn check(&mut self) -> Result<()> {
        check_claude_attachment(
            &self.client,
            &self.control.owner.subject,
            &self.control.owner.incarnation,
            &self.mailbox,
            Instant::now() - Duration::from_secs(30),
            &mut self.state,
        )
        .await
    }

    fn codes(&self) -> Vec<String> {
        self.control
            .store
            .claims_for(&self.control.owner.subject, Some("harness.diagnostic"))
            .unwrap()
            .into_iter()
            .map(|r| r.body["fields"]["code"].as_str().unwrap().into())
            .collect()
    }

    fn unchanged_owner(&self) {
        assert_eq!(
            serde_json::to_value(&self.state.mailbox_fence).unwrap(),
            serde_json::to_value(Some(&self.control.owner)).unwrap()
        );
        assert_eq!(
            serde_json::to_value(&self.mailbox.fence).unwrap(),
            serde_json::to_value(&self.control.owner).unwrap()
        );
        for request in self.control.requests.lock().unwrap().iter() {
            assert_eq!(request["subject"], self.control.owner.subject);
            assert_eq!(
                request["fields"]["incarnation_id"],
                self.control.owner.incarnation
            );
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

#[tokio::test]
async fn claude_attachment_publication_committed_lost_response_then_positive_is_exactly_once() {
    let mut f = Fixture::new().await;
    f.check().await.unwrap();
    f.control.attachment.store(0, Ordering::SeqCst);
    f.control.publication.store(2, Ordering::SeqCst);
    assert!(f.check().await.is_err());
    assert_eq!(f.state.claude_attachment_phase, "attached");
    assert_eq!(f.state.claude_attachment_episode, 1);
    assert!(f.state.claude_attachment_pending.is_some());
    assert_eq!(
        f.codes(),
        ["claude-channel-attached", "claude-channel-unattached"]
    );
    f.control.attachment.store(1, Ordering::SeqCst);
    f.check().await.unwrap();
    let requests = f.control.requests.lock().unwrap();
    assert_eq!(
        requests[1], requests[2],
        "resolve the committed operation without changing its key or payload"
    );
    assert_ne!(
        requests[2]["idempotency_key"],
        requests[3]["idempotency_key"]
    );
    drop(requests);
    assert_eq!(
        f.codes(),
        [
            "claude-channel-attached",
            "claude-channel-unattached",
            "claude-channel-attached"
        ]
    );
    assert_eq!(f.state.claude_attachment_episode, 3);
    assert!(f.state.claude_attachment_pending.is_none());
    f.check().await.unwrap();
    assert_eq!(f.codes().len(), 3);
    f.unchanged_owner();
}

#[tokio::test]
async fn claude_attachment_publication_failed_without_commit_resolves_before_recovery() {
    let mut f = Fixture::new().await;
    f.check().await.unwrap();
    f.control.attachment.store(0, Ordering::SeqCst);
    f.control.publication.store(1, Ordering::SeqCst);
    assert!(f.check().await.is_err());
    assert_eq!(f.codes(), ["claude-channel-attached"]);
    assert_eq!(f.state.claude_attachment_episode, 1);
    f.control.attachment.store(1, Ordering::SeqCst);
    f.check().await.unwrap();
    let requests = f.control.requests.lock().unwrap();
    assert_eq!(requests[1], requests[2]);
    drop(requests);
    assert_eq!(
        f.codes(),
        [
            "claude-channel-attached",
            "claude-channel-unattached",
            "claude-channel-attached"
        ]
    );
    f.unchanged_owner();
}

#[tokio::test]
async fn claude_attachment_publication_pending_survives_real_resume_serialization() {
    let mut f = Fixture::new().await;
    f.check().await.unwrap();
    f.control.attachment.store(0, Ordering::SeqCst);
    f.control.publication.store(2, Ordering::SeqCst);
    assert!(f.check().await.is_err());
    let root = tempfile::tempdir().unwrap();
    let resume = DriverResume {
        driver: "claude".into(),
        subject: f.control.owner.subject.clone(),
        incarnation: f.control.owner.incarnation.clone(),
        session: st_drivers::provider_session::DetachedSession::Provider {
            pid: 42,
            session: "quartz-session".into(),
            seq: 3,
        },
        loop_state: std::mem::take(&mut f.state),
    };
    let pending = serde_json::to_value(&resume.loop_state.claude_attachment_pending).unwrap();
    let path = st_drivers::reexec::write_state(root.path(), "driver-resume", &resume).unwrap();
    let restored: DriverResume = st_drivers::reexec::read_state(&path).unwrap();
    assert_eq!(restored.incarnation, resume.incarnation);
    assert_eq!(restored.session, resume.session);
    assert_eq!(
        serde_json::to_value(&restored.loop_state.claude_attachment_pending).unwrap(),
        pending
    );
    assert_eq!(restored.loop_state.claude_attachment_episode, 1);
    f.state = restored.loop_state;
    f.control.attachment.store(1, Ordering::SeqCst);
    f.check().await.unwrap();
    assert_eq!(
        f.codes(),
        [
            "claude-channel-attached",
            "claude-channel-unattached",
            "claude-channel-attached"
        ]
    );
    f.unchanged_owner();
}

#[tokio::test]
async fn claude_attachment_publication_reason_change_cannot_mutate_uncertain_request() {
    let mut f = Fixture::new().await;
    f.check().await.unwrap();
    f.control.attachment.store(0, Ordering::SeqCst);
    f.control.publication.store(2, Ordering::SeqCst);
    assert!(f.check().await.is_err());
    let pending = serde_json::to_value(&f.state.claude_attachment_pending).unwrap();
    f.control.attachment.store(2, Ordering::SeqCst); // The current failure reason now differs.
    f.control.publication.store(1, Ordering::SeqCst);
    assert!(f.check().await.is_err());
    assert_eq!(
        serde_json::to_value(&f.state.claude_attachment_pending).unwrap(),
        pending
    );
    assert_eq!(f.state.claude_attachment_episode, 1);
    f.check().await.unwrap();
    let requests = f.control.requests.lock().unwrap();
    assert_eq!(requests[1], requests[2]);
    assert_eq!(requests[2], requests[3]);
    drop(requests);
    assert_eq!(
        f.codes(),
        ["claude-channel-attached", "claude-channel-unattached"]
    );
    f.control.attachment.store(1, Ordering::SeqCst);
    f.check().await.unwrap();
    assert_eq!(f.codes().len(), 3);
    f.unchanged_owner();
}

#[tokio::test]
async fn claude_attachment_publication_rechecks_inherited_phase_and_rejects_foreign_pending() {
    let mut f = Fixture::new().await;
    f.check().await.unwrap();
    f.control.attachment.store(0, Ordering::SeqCst);
    f.control.publication.store(2, Ordering::SeqCst);
    assert!(f.check().await.is_err());
    let mut inherited = serde_json::to_value(&f.state).unwrap();
    inherited
        .as_object_mut()
        .unwrap()
        .remove("claude_attachment_pending"); // Predecessor wire format.
    f.state = serde_json::from_value(inherited).unwrap();
    f.control.attachment.store(1, Ordering::SeqCst);
    f.check().await.unwrap();
    assert_eq!(f.codes().last().unwrap(), "claude-channel-attached");
    f.control.attachment.store(0, Ordering::SeqCst);
    f.control.publication.store(1, Ordering::SeqCst);
    assert!(f.check().await.is_err());
    let requests = f.control.requests.lock().unwrap().len();
    f.mailbox.fence.epoch += 1;
    assert!(
        f.check()
            .await
            .unwrap_err()
            .to_string()
            .contains("another mailbox binding")
    );
    assert_eq!(f.control.requests.lock().unwrap().len(), requests);
    f.mailbox.fence = f.control.owner.clone();
    f.control.attachment.store(1, Ordering::SeqCst);
    f.check().await.unwrap();
    f.unchanged_owner();
}

#[tokio::test]
async fn claude_attachment_publication_does_not_clear_other_harness_axes() {
    for (state, diagnostic) in [
        ("blocked", None),
        ("ended", None),
        ("indeterminate", None),
        ("idle", Some("provider-auth-expired")),
        ("idle", Some("provider-update-prompt")),
    ] {
        let mut f = Fixture::new().await;
        f.control
            .store
            .append_claim(&ClaimInput {
                subject: f.control.owner.subject.clone(),
                kind: "harness.observed".into(),
                actor: Some(f.control.owner.subject.clone()),
                fields: BTreeMap::from([
                    ("state".into(), json!(state)),
                    ("driver".into(), json!("claude")),
                    ("blocked_on".into(), json!("human")),
                    ("incarnation_id".into(), json!(f.control.owner.incarnation)),
                ]),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        if let Some(code) = diagnostic {
            f.control
                .store
                .append_claim(&ClaimInput {
                    subject: f.control.owner.subject.clone(),
                    kind: "harness.diagnostic".into(),
                    actor: Some(f.control.owner.subject.clone()),
                    fields: BTreeMap::from([
                        ("code".into(), json!(code)),
                        ("incarnation_id".into(), json!(f.control.owner.incarnation)),
                    ]),
                    evidence: vec![],
                    expected_subject: None,
                    idempotency_key: None,
                })
                .unwrap();
        }
        let before = f
            .control
            .store
            .current_harness(&f.control.owner.subject)
            .unwrap()
            .unwrap();
        f.control.attachment.store(0, Ordering::SeqCst);
        f.control.publication.store(2, Ordering::SeqCst);
        assert!(f.check().await.is_err());
        f.control.attachment.store(1, Ordering::SeqCst);
        f.check().await.unwrap();
        let after = f
            .control
            .store
            .current_harness(&f.control.owner.subject)
            .unwrap()
            .unwrap();
        assert_eq!(after.state, before.state);
        assert_eq!(after.blocked_on, before.blocked_on);
        assert_eq!(after.reason, before.reason);
        assert_eq!(after.claim, before.claim);
        assert_eq!(
            after.observed_at_unix_ms, before.observed_at_unix_ms,
            "publication does not manufacture harness freshness"
        );
        f.unchanged_owner();
    }
}
