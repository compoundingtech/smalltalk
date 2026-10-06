//! Publication controls use an isolated Unix HTTP/WebSocket endpoint and the real graph's idempotency fold.
//! They never launch a provider or modify a live seat.
use super::*;
use axum::{
    Json, Router,
    extract::{Query, State, WebSocketUpgrade},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use std::sync::{
    Mutex,
    atomic::{AtomicBool, AtomicU8, Ordering},
};

struct Control {
    store: Store,
    owner: st3::mailbox::Fence,
    binding_live: AtomicBool,
    attachment_failure_once: AtomicBool,
    permanent: Mutex<Option<(u16, String)>>,
    reports: Mutex<Vec<Value>>,
    report_changed: Notify,
    attachment: AtomicU8,  // 0: absent, 1: admitted, 2: failed check
    publication: AtomicU8, // 0: acknowledged, 1: no commit, 2: commit without acknowledgement
    requests: Mutex<Vec<Value>>,
}

async fn attachment(
    State(control): State<Arc<Control>>,
    Query(fence): Query<st3::mailbox::Fence>,
) -> Response {
    let mode = control.attachment.load(Ordering::SeqCst);
    if !control.binding_live.load(Ordering::SeqCst) {
        return StatusCode::UNPROCESSABLE_ENTITY.into_response();
    }
    if mode == 2
        || control
            .attachment_failure_once
            .swap(false, Ordering::SeqCst)
    {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let owns =
        serde_json::to_value(&fence).unwrap() == serde_json::to_value(&control.owner).unwrap();
    if !owns {
        return StatusCode::UNPROCESSABLE_ENTITY.into_response();
    }
    Json(json!({"api_version":"st3.v1", "value":{"attached": mode == 1}})).into_response()
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
    if let Some((status, code)) = control.permanent.lock().unwrap().take() {
        return (
            StatusCode::from_u16(status).unwrap(),
            Json(json!({"code":code,"message":"permanent refusal","details":{}})),
        )
            .into_response();
    }
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

async fn mailbox_reports(State(control): State<Arc<Control>>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |mut socket| async move {
        while let Some(Ok(axum::extract::ws::Message::Text(text))) = socket.recv().await {
            control
                .reports
                .lock()
                .unwrap()
                .push(serde_json::from_str(&text).unwrap());
            control.report_changed.notify_one();
        }
    })
}

struct Fixture {
    control: Arc<Control>,
    client: Client,
    mailbox: NativeMailbox,
    state: NativeLoopState,
    server: tokio::task::JoinHandle<()>,
    _root: tempfile::TempDir,
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
            binding_live: AtomicBool::new(true),
            attachment_failure_once: AtomicBool::new(false),
            permanent: Mutex::new(None),
            reports: Mutex::new(vec![]),
            report_changed: Notify::new(),
            attachment: AtomicU8::new(1),
            publication: AtomicU8::new(0),
            requests: Mutex::new(vec![]),
        });
        let app = Router::new()
            .route("/v1/mailbox/attachment", get(attachment))
            .route("/v1/mailbox", get(mailbox_reports))
            .route("/v1/claims", post(publication))
            .with_state(control.clone());
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("api.sock");
        let client = Client::unix(&socket);
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            st3::api::serve_unix_with_ready(&socket, app, || {
                let _ = ready_tx.send(());
            })
            .await
            .unwrap();
        });
        ready_rx.await.unwrap();
        let subscription = st3::mailbox::Subscription::start(
            client.clone(),
            owner.clone(),
            json!({"ready":false}),
        );
        Self {
            state: NativeLoopState {
                mailbox_fence: Some(owner.clone()),
                ..Default::default()
            },
            mailbox: NativeMailbox {
                subscription: Some(subscription),
                fence: owner,
                messages: vec![],
                queued: BTreeMap::new(),
                replayed: false,
                last_title_warning: None,
            },
            control,
            client,
            server,
            _root: root,
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

    async fn wait_ready(&self, ready: bool) {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let changed = self.control.report_changed.notified();
                if self
                    .control
                    .reports
                    .lock()
                    .unwrap()
                    .last()
                    .is_some_and(|v| v["ready"] == ready)
                {
                    return;
                }
                changed.await;
            }
        })
        .await
        .unwrap();
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
    assert!(!restored.loop_state.claude_attachment_reconciled);
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
    // The reporting GET fails once, so its current reason differs. The following
    // binding validation succeeds and the pending POST must still use its old reason.
    f.control
        .attachment_failure_once
        .store(true, Ordering::SeqCst);
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
async fn claude_attachment_publication_legacy_uncertain_phase_requires_one_correction() {
    let mut f = Fixture::new().await;
    f.check().await.unwrap();
    f.control.attachment.store(0, Ordering::SeqCst);
    f.control.publication.store(2, Ordering::SeqCst);
    assert!(f.check().await.is_err());
    let mut inherited = serde_json::to_value(&f.state).unwrap();
    for field in [
        "claude_attachment_pending",
        "claude_attachment_reconciled",
        "claude_attachment_terminal",
    ] {
        inherited.as_object_mut().unwrap().remove(field);
    }
    f.state = serde_json::from_value(inherited).unwrap();
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
    assert_eq!(f.state.claude_attachment_episode, 2);
    f.check().await.unwrap();
    assert_eq!(f.codes().len(), 3);
    assert_eq!(f.state.claude_attachment_episode, 2);
    f.unchanged_owner();
}

#[tokio::test]
async fn claude_attachment_publication_acknowledged_resume_does_not_republish() {
    let mut f = Fixture::new().await;
    f.check().await.unwrap();
    assert!(f.state.claude_attachment_reconciled);
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
    let path = st_drivers::reexec::write_state(root.path(), "driver-resume", &resume).unwrap();
    let restored: DriverResume = st_drivers::reexec::read_state(&path).unwrap();
    f.state = restored.loop_state;
    assert!(f.state.claude_attachment_reconciled);
    f.check().await.unwrap();
    assert_eq!(f.codes(), ["claude-channel-attached"]);
    assert_eq!(f.control.requests.lock().unwrap().len(), 1);
    assert_eq!(f.state.claude_attachment_episode, 1);
    f.unchanged_owner();
}

#[tokio::test]
async fn claude_attachment_publication_readiness_reports_despite_repeated_post_failure() {
    let mut f = Fixture::new().await;
    f.check().await.unwrap();
    f.wait_ready(true).await;
    f.control.attachment.store(0, Ordering::SeqCst);
    f.control.publication.store(2, Ordering::SeqCst);
    assert!(f.check().await.is_err());
    f.wait_ready(false).await;
    f.control.attachment.store(1, Ordering::SeqCst);
    f.control.publication.store(1, Ordering::SeqCst);
    assert!(f.check().await.is_err());
    f.wait_ready(true).await;
    assert_eq!(f.state.claude_attachment_episode, 1);
    assert!(f.state.claude_attachment_pending.is_some());
    let requests = f.control.requests.lock().unwrap();
    assert_eq!(requests[1], requests[2]);
    drop(requests);
    f.unchanged_owner();
}

#[tokio::test]
async fn claude_attachment_publication_terminal_api_errors_are_capped_without_ack() {
    for (status, code) in [
        (422, "idempotency-mismatch"),
        (409, "claim-checkpointed"),
        (400, "bad-request"),
        (401, "unauthorized"),
        (403, "forbidden"),
        (404, "not-found"),
        (429, "rate-limited"),
        (500, "idempotency-mismatch"),
        (500, "claim-checkpointed"),
    ] {
        let mut f = Fixture::new().await;
        f.check().await.unwrap();
        f.control.attachment.store(0, Ordering::SeqCst);
        *f.control.permanent.lock().unwrap() = Some((status, code.into()));
        let error = f.check().await.unwrap_err();
        assert!(error.to_string().contains("publication stopped"));
        assert_eq!(f.state.claude_attachment_episode, 1);
        assert_eq!(f.state.claude_attachment_phase, "attached");
        assert!(f.state.claude_attachment_pending.is_none());
        assert!(
            f.state
                .claude_attachment_terminal
                .as_ref()
                .unwrap()
                .reason
                .contains(code)
        );
        f.wait_ready(false).await;
        f.state = serde_json::from_value(serde_json::to_value(&f.state).unwrap()).unwrap();
        f.control.attachment.store(1, Ordering::SeqCst);
        for _ in 0..3 {
            f.check().await.unwrap();
        }
        f.wait_ready(true).await;
        assert_eq!(f.control.requests.lock().unwrap().len(), 2);
        assert_eq!(f.codes(), ["claude-channel-attached"]);
        assert_eq!(f.state.claude_attachment_episode, 1);
        f.unchanged_owner();
    }
}

#[tokio::test]
async fn claude_attachment_publication_foreign_or_malformed_pending_never_posts() {
    for field in [
        "subject",
        "incarnation",
        "component",
        "epoch",
        "token",
        "actor",
        "input-subject",
        "input-incarnation",
        "key",
        "kind",
        "phase",
        "reason",
        "missing-reason",
        "digest",
    ] {
        let mut f = Fixture::new().await;
        f.check().await.unwrap();
        f.control.attachment.store(0, Ordering::SeqCst);
        f.control.publication.store(1, Ordering::SeqCst);
        assert!(f.check().await.is_err());
        let pending = f.state.claude_attachment_pending.as_mut().unwrap();
        match field {
            "subject" => pending.fence.subject.push_str("-foreign"),
            "incarnation" => pending.fence.incarnation.push_str("-foreign"),
            "component" => pending.fence.component = "delivery".into(),
            "epoch" => pending.fence.epoch += 1,
            "token" => pending.fence.token.push_str("-foreign"),
            "actor" => pending.input.actor = Some("agent/foreign".into()),
            "input-subject" => pending.input.subject = "agent/foreign".into(),
            "input-incarnation" => {
                pending
                    .input
                    .fields
                    .insert("incarnation_id".into(), json!("8:foreign"));
            }
            "key" => pending.input.idempotency_key = Some("foreign-key".into()),
            "kind" => pending.input.kind = "harness.observed".into(),
            "phase" => pending.phase = "invalid".into(),
            "reason" => {
                pending
                    .input
                    .fields
                    .insert("reason".into(), json!("mutated reason"));
            }
            "missing-reason" => {
                pending.input.fields.remove("reason");
            }
            "digest" => pending.input_digest = "invalid".into(),
            _ => unreachable!(),
        }
        if matches!(field, "key" | "kind") {
            pending.input_digest = claude_attachment_input_digest(&pending.input).unwrap();
        }
        assert!(
            f.check()
                .await
                .unwrap_err()
                .to_string()
                .contains("publication stopped"),
            "{field}"
        );
        assert!(f.state.claude_attachment_pending.is_none());
        assert_eq!(f.state.claude_attachment_episode, 1);
        for _ in 0..3 {
            f.check().await.unwrap();
        }
        assert_eq!(f.control.requests.lock().unwrap().len(), 2, "{field}");
        assert_eq!(f.codes(), ["claude-channel-attached"]);
        f.unchanged_owner();
    }
}

#[tokio::test]
async fn claude_attachment_publication_superseded_server_binding_never_retries_post() {
    let mut f = Fixture::new().await;
    f.check().await.unwrap();
    f.control.attachment.store(0, Ordering::SeqCst);
    f.control.publication.store(1, Ordering::SeqCst);
    assert!(f.check().await.is_err());
    f.control.binding_live.store(false, Ordering::SeqCst);
    assert!(
        f.check()
            .await
            .unwrap_err()
            .to_string()
            .contains("publication stopped")
    );
    assert_eq!(f.state.claude_attachment_episode, 1);
    assert!(f.state.claude_attachment_pending.is_none());
    for _ in 0..3 {
        f.check().await.unwrap();
    }
    assert_eq!(f.control.requests.lock().unwrap().len(), 2);
    f.wait_ready(false).await;
    f.unchanged_owner();
}

#[tokio::test]
async fn claude_attachment_publication_overflow_refuses_before_post() {
    let mut f = Fixture::new().await;
    f.state.claude_attachment_episode = u64::MAX;
    assert!(
        f.check()
            .await
            .unwrap_err()
            .to_string()
            .contains("overflowed")
    );
    assert_eq!(f.state.claude_attachment_episode, u64::MAX);
    assert!(f.state.claude_attachment_pending.is_none());
    assert!(f.control.requests.lock().unwrap().is_empty());
    f.check().await.unwrap();
    assert!(f.control.requests.lock().unwrap().is_empty());
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
