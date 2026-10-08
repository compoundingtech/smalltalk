use super::*;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

fn append(store: &Store, subject: &str) {
    store
        .append_claim(&ClaimInput {
            subject: subject.into(),
            kind: "example.note".into(),
            actor: None,
            fields: BTreeMap::from([("text".into(), Value::String(subject.into()))]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn credit_rearms_only_on_signed_success_and_invalidates_route_or_auth_epochs() {
    let fleet = FleetContext::legacy(BTreeSet::from(["cedar".into()]));
    let auth = FleetAuth::test("invented-fleet", &[7; 32]);
    let (tx, mut routes) = watch::channel(vec![Route::Http("http://cedar".into())]);
    let mut credit = TransportRecoveryCredit::default();
    assert_eq!(
        credit.refusal("http://cedar", true, true),
        Some("recovery-refused-epoch"),
        "restart starts without credit"
    );
    credit.grant("http://cedar", Some("old".into()), &auth, &fleet, "cedar");
    credit.validate(&routes, &auth, &fleet, "cedar");
    assert_eq!(credit.refusal("http://cedar", true, true), None);
    credit.spend();
    for _ in 0..3 {
        fleet.connectivity_changed.send_modify(|v| *v += 1);
        credit.validate(&routes, &auth, &fleet, "cedar");
        assert_eq!(
            credit.refusal("http://cedar", true, true),
            Some("recovery-refused-spent")
        );
    }
    // Only a successful signed outbound exchange uses grant.
    credit.grant(
        "http://cedar",
        Some("acknowledged".into()),
        &auth,
        &fleet,
        "cedar",
    );
    assert_eq!(credit.refusal("http://cedar", true, true), None);
    tx.send(vec![]).unwrap();
    tx.send(vec![Route::Http("http://cedar".into())]).unwrap();
    credit.validate(&routes, &auth, &fleet, "cedar");
    routes.borrow_and_update();
    assert!(
        credit.epoch.is_none(),
        "coalesced remove/readd is not a new authenticated epoch"
    );
    credit.validate(&routes, &auth, &fleet, "cedar");
    assert!(credit.epoch.is_none());
    credit.grant(
        "http://cedar",
        Some("acknowledged".into()),
        &auth,
        &fleet,
        "cedar",
    );
    let rotated = auth
        .clone()
        .with_member_key(Some(Arc::new(MemberKey::generate().unwrap().0)));
    credit.validate(&routes, &rotated, &fleet, "cedar");
    assert!(
        credit.epoch.is_none(),
        "signing-key change invalidates credit"
    );
    credit.grant(
        "http://cedar",
        Some("acknowledged".into()),
        &auth,
        &fleet,
        "cedar",
    );
    let captured_before_dispatch = TransportRecoveryCredit::auth_epoch(&auth, &fleet, "cedar");
    let rotated_secret = FleetAuth::test("invented-fleet", &[8; 32]);
    credit.grant_epoch(
        "http://cedar",
        Some("acknowledged".into()),
        captured_before_dispatch,
    );

    credit.validate(&routes, &rotated_secret, &fleet, "cedar");
    assert!(
        credit.epoch.is_none(),
        "fleet signing-secret change invalidates credit"
    );
    credit.grant(
        "http://cedar",
        Some("acknowledged".into()),
        &auth,
        &fleet,
        "cedar",
    );
    fleet.view_changed.send_modify(|v| *v += 1);
    credit.validate(&routes, &auth, &fleet, "cedar");
    assert!(
        credit.epoch.is_none(),
        "membership/key epoch change invalidates credit"
    );
    credit.grant(
        "http://cedar",
        Some("acknowledged".into()),
        &auth,
        &fleet,
        "cedar",
    );
    fleet.view.write().unwrap().members.push(
        serde_json::from_value(serde_json::json!({
            "name":"cedar", "member_key":"key-one", "state":"current", "mode":"listen", "start":1
        }))
        .unwrap(),
    );
    credit.grant(
        "http://cedar",
        Some("acknowledged".into()),
        &auth,
        &fleet,
        "cedar",
    );
    fleet.view.write().unwrap().members[0].member_key = "key-two".into();
    credit.validate(&routes, &auth, &fleet, "cedar");
    assert!(
        credit.epoch.is_none(),
        "peer signing-key rotation invalidates credit"
    );
    credit.grant(
        "http://cedar",
        Some("acknowledged".into()),
        &auth,
        &fleet,
        "cedar",
    );
    fleet.view.write().unwrap().members[0].state = "ended".into();
    credit.validate(&routes, &auth, &fleet, "cedar");
    assert!(credit.epoch.is_none(), "peer removal invalidates credit");
    credit.grant(
        "http://cedar",
        Some("acknowledged".into()),
        &auth,
        &fleet,
        "cedar",
    );
    tokio::time::advance(PEER_PROBE_WINDOW).await;
    credit.validate(&routes, &auth, &fleet, "cedar");
    assert!(
        credit.epoch.is_none(),
        "stale authenticated epoch cannot recover"
    );
    credit.grant(
        "http://cedar",
        Some("acknowledged".into()),
        &auth,
        &fleet,
        "cedar",
    );
    fleet.removed.store(true, Ordering::Release);
    credit.validate(&routes, &auth, &fleet, "cedar");
    assert!(credit.epoch.is_none());
}

#[tokio::test(start_paused = true)]
async fn only_newer_authority_admits_recovery_despite_quiet_inbound_and_graph_wakes() {
    let fleet = FleetContext::legacy(BTreeSet::from(["cedar".into()]));
    let auth = FleetAuth::test("invented-fleet", &[7; 32]);
    let store = Arc::new(plain_memory("birch").unwrap());
    store.bind_fleet(auth.fleet_id()).unwrap();
    let local = Local(store.clone());
    let old = store
        .export_replication_summary(auth.fleet_id())
        .unwrap()
        .authority_digest;
    let mut credit = TransportRecoveryCredit::default();
    credit.grant("http://cedar", Some(old.clone()), &auth, &fleet, "cedar");
    fleet
        .inbound_authority
        .write()
        .unwrap()
        .insert("cedar".into(), old);
    fleet
        .inbound
        .write()
        .unwrap()
        .insert("cedar".into(), tokio::time::Instant::now());
    assert!(
        !pending_peer_authority(&local, &auth, &fleet, "cedar", &credit).await,
        "a graph wake/HEAD alone is not newer authority"
    );
    append(&store, "note/new-pending");
    assert!(
        pending_peer_authority(&local, &auth, &fleet, "cedar", &credit).await,
        "recent inbound does not cover genuinely newer local authority"
    );
    let now = store
        .export_replication_summary(auth.fleet_id())
        .unwrap()
        .authority_digest;
    credit.grant("http://cedar", Some(now.clone()), &auth, &fleet, "cedar");
    assert!(
        !pending_peer_authority(&local, &auth, &fleet, "cedar", &credit).await,
        "newer authenticated outbound acknowledgement overrides stale inbound digest"
    );
    fleet
        .inbound_authority
        .write()
        .unwrap()
        .insert("cedar".into(), now);
    assert!(!pending_peer_authority(&local, &auth, &fleet, "cedar", &credit).await);
    assert_eq!(
        credit.refusal("http://cedar", true, false),
        Some("recovery-refused-no-pending")
    );
    assert_eq!(
        credit.refusal("http://other", true, true),
        Some("recovery-refused-route")
    );
    assert_eq!(
        credit.refusal("http://cedar", false, true),
        Some("recovery-refused-cause")
    );
}

#[tokio::test]
async fn head405_admits_one_jittered_attempt_then_resumes_original_counter_deadline_and_probe_rate()
{
    let probes = Arc::new(AtomicUsize::new(0));
    let count = probes.clone();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(
        axum::serve(
            listener,
            Router::new().route(
                EXCHANGE_PATH,
                axum::routing::head(move || {
                    count.fetch_add(1, Ordering::Relaxed);
                    async { StatusCode::METHOD_NOT_ALLOWED }
                })
                .post(|| async { StatusCode::UNAUTHORIZED }),
            ),
        )
        .into_future(),
    );
    let fleet = FleetContext::legacy(BTreeSet::from(["cedar".into()]));
    let auth = FleetAuth::test("invented-fleet", &[7; 32]);
    let store = Arc::new(plain_memory("birch").unwrap());
    store.bind_fleet(auth.fleet_id()).unwrap();
    let mut credit = TransportRecoveryCredit::default();
    credit.grant(
        &url,
        Some(
            store
                .export_replication_summary(auth.fleet_id())
                .unwrap()
                .authority_digest,
        ),
        &auth,
        &fleet,
        "cedar",
    );
    append(&store, "note/new");
    let local = Local(store);
    let (_tx, mut routes) = watch::channel(vec![Route::Http(url.clone())]);
    let mut inbound = fleet.inbound_changed.subscribe();
    let mut activity = fleet.activity_changed.subscribe();
    let mut connectivity = fleet.connectivity_changed.subscribe();
    let http = replication_http_client();
    let previous = (url.clone(), tokio::time::Instant::now());
    let started = tokio::time::Instant::now();
    let mut window = PeerRetryWindow::new(Duration::from_secs(30));
    let deadline = window.deadline;
    let mut backoff = PeerBackoff { failures: 7 };
    let outcome = wait_peer_retry_inner(
        &local,
        crate::store::now_ms(),
        &mut window,
        &mut routes,
        &mut inbound,
        &mut activity,
        &mut connectivity,
        &fleet,
        "cedar",
        started,
        Some(&previous),
        &http,
        Some(RecoveryOpportunity {
            credit: &mut credit,
            auth: &auth,
            failed_url: &url,
            eligible: true,
            pending: true,
        }),
    )
    .await;
    assert_eq!(outcome, PeerRetryOutcome::TransportRecovery);
    assert!(started.elapsed() < Duration::from_secs(30));
    assert!(started.elapsed() >= PEER_PROBE_INTERVAL + Duration::from_millis(100));
    assert_eq!(window.deadline, deadline);
    assert_eq!(probes.load(Ordering::Relaxed), 1);
    assert!(fleet.activity.read().unwrap().is_empty());
    assert!(fleet.inbound.read().unwrap().is_empty());
    // A signed attempt can fail after the HEAD. It must not qualify as transport.
    let query = local.0.export_replication_summary(auth.fleet_id()).unwrap();
    let peer = PeerConfig {
        name: "cedar".into(),
        url: url.clone(),
    };
    let error = post_signed(&http, &local, &peer, "birch", &auth, &fleet, &query, false)
        .await
        .unwrap_err();
    assert!(!is_transport_recovery_failure(&error));
    // Drive the remaining original wait cheaply with a paused clock, preserving I/O.
    tokio::time::pause();
    let awake = tokio::spawn(async {
        loop {
            tokio::task::yield_now().await;
        }
    });
    {
        let wait = wait_peer_retry_inner(
            &local,
            crate::store::now_ms(),
            &mut window,
            &mut routes,
            &mut inbound,
            &mut activity,
            &mut connectivity,
            &fleet,
            "cedar",
            tokio::time::Instant::now(),
            Some(&previous),
            &http,
            Some(RecoveryOpportunity {
                credit: &mut credit,
                auth: &auth,
                failed_url: &url,
                eligible: true,
                pending: true,
            }),
        );
        tokio::pin!(wait);
        let mut done = false;
        for _ in 0..35 {
            tokio::select! {
                outcome = &mut wait => { assert_eq!(outcome, PeerRetryOutcome::Deadline); done = true; break; }
                _ = async { for _ in 0..100 { tokio::task::yield_now().await; } } => {}
            }
            tokio::time::advance(Duration::from_secs(1)).await;
        }
        assert!(done);
    }

    let resumed = backoff.window(
        Some(PeerRetryWindow {
            started,
            deadline,
            next_probe: started + PEER_PROBE_INTERVAL,
        }),
        &fleet,
        "cedar",
    );
    assert_eq!(resumed.deadline, deadline);
    assert_eq!(
        backoff.failures, 7,
        "resuming the production wait did not call next/reset"
    );
    // Ordinary retry policy remains jittered and grows normally on the next window.
    let _ = backoff.next();
    assert_eq!(backoff.failures, 8);
    assert!(probes.load(Ordering::Relaxed) <= 30 / PEER_PROBE_INTERVAL.as_secs() as usize);
    awake.abort();
    server.abort();
}

#[tokio::test]
async fn accepted_request_with_lost_headers_replays_idempotently() {
    let auth = FleetAuth::test("invented-fleet", &[7; 32]);
    let left = Arc::new(plain_memory("birch").unwrap());
    let right = Arc::new(plain_memory("cedar").unwrap());
    for store in [&left, &right] {
        store.bind_fleet(auth.fleet_id()).unwrap();
    }
    append(&left, "note/once");
    let state = PeerState::new(
        Local(right.clone()),
        "cedar".into(),
        auth.clone(),
        FleetContext::legacy(BTreeSet::from(["birch".into()])),
    );
    let drop_response = Arc::new(AtomicBool::new(true));
    let drop_once = drop_response.clone();
    let requests = Arc::new(AtomicUsize::new(0));
    let count = requests.clone();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let peer = PeerConfig {
        name: "cedar".into(),
        url: format!("http://{}", listener.local_addr().unwrap()),
    };
    let server = tokio::spawn(
        axum::serve(
            listener,
            Router::new().route(
                EXCHANGE_PATH,
                post(move |headers: HeaderMap, body: Bytes| {
                    let state = state.clone();
                    let drop_once = drop_once.clone();
                    let count = count.clone();
                    async move {
                        count.fetch_add(1, Ordering::Relaxed);
                        let response = receive_exchange(State(state), headers, body).await;
                        assert_eq!(response.status(), StatusCode::OK);
                        if drop_once.swap(false, Ordering::Relaxed) {
                            std::future::pending::<()>().await;
                        }
                        response
                    }
                }),
            ),
        )
        .into_future(),
    );
    let http = reqwest::Client::builder()
        .timeout(Duration::from_millis(500))
        .build()
        .unwrap();
    let fleet = FleetContext::legacy(BTreeSet::from(["cedar".into()]));
    let query = left
        .export_replication_exchange(auth.fleet_id(), &ReplicationInventory::default())
        .unwrap();
    let error = post_signed_to::<_, ReplicationExchange>(
        &http,
        &peer,
        "birch",
        &auth,
        &fleet,
        EXCHANGE_PATH,
        &query,
        false,
    )
    .await
    .unwrap_err();
    assert!(is_transport_recovery_failure(&error));
    assert!(
        error
            .downcast_ref::<PreHeadersTransportFailure>()
            .unwrap()
            .source
            .is_timeout()
    );
    assert_eq!(
        right
            .claims_for("note/once", Some("example.note"))
            .unwrap()
            .len(),
        1,
        "before-headers timeout did not mean nonexecution"
    );
    post_signed_to::<_, ReplicationExchange>(
        &http,
        &peer,
        "birch",
        &auth,
        &fleet,
        EXCHANGE_PATH,
        &query,
        false,
    )
    .await
    .unwrap();
    assert_eq!(requests.load(Ordering::Relaxed), 2);
    assert_eq!(
        right
            .claims_for("note/once", Some("example.note"))
            .unwrap()
            .len(),
        1
    );
    server.abort();
}

#[tokio::test(start_paused = true)]
async fn cancelled_recovery_jitter_stays_spent_and_keeps_the_original_window() {
    let fleet = FleetContext::legacy(BTreeSet::from(["cedar".into()]));
    let auth = FleetAuth::test("invented-fleet", &[7; 32]);
    let backend = Local(Arc::new(plain_memory("birch").unwrap()));
    let mut credit = TransportRecoveryCredit::default();
    credit.grant("http://cedar", Some("old".into()), &auth, &fleet, "cedar");
    let mut backoff = PeerBackoff { failures: 7 };
    let window = PeerRetryWindow::new(Duration::from_secs(30));
    let deadline = window.deadline;
    assert!(
        tokio::time::timeout(
            Duration::from_millis(50),
            consume_recovery_credit(
                &mut credit,
                &backend,
                "cedar",
                crate::store::now_ms(),
                &window
            )
        )
        .await
        .is_err()
    );
    assert_eq!(
        credit.refusal("http://cedar", true, true),
        Some("recovery-refused-spent")
    );
    let resumed = backoff.window(Some(window), &fleet, "cedar");
    assert_eq!(resumed.deadline, deadline);
    assert_eq!(backoff.failures, 7);
    assert_eq!(resumed.remaining(), Duration::from_millis(29950));
    for jitter in [0, 100, 400, u16::MAX] {
        assert!(recovery_jitter(jitter) >= Duration::from_millis(100));
        assert!(recovery_jitter(jitter) <= Duration::from_millis(500));
    }
}

#[tokio::test]
async fn response_status_auth_body_and_typed_refusals_are_not_transport_failures() {
    let auth = FleetAuth::test("invented-fleet", &[7; 32]);
    let fleet = FleetContext::legacy(BTreeSet::from(["cedar".into()]));
    let store = plain_memory("birch").unwrap();
    store.bind_fleet(auth.fleet_id()).unwrap();
    let query = store.export_replication_summary(auth.fleet_id()).unwrap();
    for answer in [
        "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n",
        "HTTP/1.1 503 Unavailable\r\nContent-Length: 0\r\n\r\n",
        "HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\nx",
    ] {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let peer = PeerConfig {
            name: "cedar".into(),
            url: format!("http://{}", listener.local_addr().unwrap()),
        };
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = [0; 65536];
            let _ = socket.read(&mut buffer).await.unwrap();
            socket.write_all(answer.as_bytes()).await.unwrap();
            socket.shutdown().await.unwrap();
        });
        let error = post_signed_to::<_, ReplicationExchange>(
            &replication_http_client(),
            &peer,
            "birch",
            &auth,
            &fleet,
            EXCHANGE_PATH,
            &query,
            false,
        )
        .await
        .unwrap_err();
        assert!(
            !is_transport_recovery_failure(&error),
            "{answer}: {error:#}"
        );
        server.await.unwrap();
    }
    for error in [
        anyhow::Error::from(PeerOverloaded {
            retry_after: Duration::from_secs(5),
        }),
        anyhow::Error::from(RemovedFromFleet {
            code: "member-removed".into(),
            message: "removed".into(),
        }),
        anyhow::Error::from(FabricGrantRefusal {
            node: "cedar".into(),
            protocol: "claims".into(),
        }),
        anyhow::Error::from(RecoveryAttemptInterrupted),
    ] {
        assert!(!is_transport_recovery_failure(&error));
    }
}

type WorkerPhaseObservation = (tokio::time::Instant, String, Option<tokio::time::Instant>);

#[derive(Clone)]
struct ObservedWorker {
    local: Local<Store>,
    phases: Arc<Mutex<Vec<WorkerPhaseObservation>>>,
}
impl Backend for ObservedWorker {
    async fn export(
        &self,
        fleet: &str,
        inventory: &ReplicationInventory,
        summary: bool,
        signatures: &[ReplicaEnvelopeId],
    ) -> Result<ReplicationExportResponse> {
        self.local
            .export(fleet, inventory, summary, signatures)
            .await
    }
    async fn receive(
        &self,
        peer: &str,
        fleet: &str,
        exchange: &ReplicationExchange,
        trip: Option<Duration>,
    ) -> Result<ReplicationReceiveResponse> {
        self.local.receive(peer, fleet, exchange, trip).await
    }
    async fn heal_answer(
        &self,
        peer: &str,
        fleet: &str,
        query: &ReplicationHealQuery,
    ) -> Result<ReplicationHealAnswer> {
        self.local.heal_answer(peer, fleet, query).await
    }
    async fn heal_next(
        &self,
        peer: &str,
        answer: ReplicationHealAnswer,
    ) -> Result<ReplicationHealStep> {
        self.local.heal_next(peer, answer).await
    }
    async fn checkpoint_manifest(
        &self,
        request: &CheckpointManifestRequest,
    ) -> Result<CheckpointManifestPage> {
        self.local.checkpoint_manifest(request).await
    }
    async fn checkpoint_need(&self) -> Result<Option<CheckpointManifestNeed>> {
        self.local.checkpoint_need().await
    }
    async fn adopt_checkpoint(
        &self,
        manifest: &CheckpointManifest,
    ) -> Result<Vec<CheckpointAction>> {
        self.local.adopt_checkpoint(manifest).await
    }
    async fn publish_endpoints(&self, mode: &str, endpoints: &[Value]) -> Result<()> {
        self.local.publish_endpoints(mode, endpoints).await
    }
    async fn redeem(&self, request: &JoinRequest) -> Result<Value> {
        self.local.redeem(request).await
    }
    async fn fleet_view(&self) -> Result<FleetView> {
        self.local.fleet_view().await
    }
    async fn record_failure(&self, peer: &str, status: &str, error: &str) -> Result<()> {
        self.local.record_failure(peer, status, error).await
    }
    async fn record_worker(
        &self,
        peer: &str,
        worker: crate::replication::ReplicationWorkerStatus,
    ) -> Result<()> {
        let at = tokio::time::Instant::now();
        let deadline = worker.next_retry_at_unix_ms.map(|deadline| {
            at + Duration::from_millis(deadline.saturating_sub(crate::store::now_ms()) as u64)
        });
        self.phases
            .lock()
            .unwrap()
            .push((at, worker.phase.clone(), deadline));
        self.local.record_worker(peer, worker).await
    }
}

/// Ordinary graph-only worker control, not the native messaging campaign or attribution
/// of a past schedule. Counter depth comes from actual failed production sends, not a seed.
#[tokio::test]
async fn production_dialer_recovers_pending_authority_after_real_failures_without_replaying_warmup()
{
    production_transport_control(false).await;
}

#[tokio::test]
async fn production_alive_head_and_failing_post_spends_only_one_credit() {
    production_transport_control(true).await;
}

async fn production_transport_control(heads_alive_during_partition: bool) {
    let auth = FleetAuth::test("invented-fleet", &[7; 32]);
    let root = tempfile::tempdir().unwrap();
    let left = Arc::new(plain_open(&root.path().join("left.sqlite3"), "birch").unwrap());
    let right = Arc::new(plain_open(&root.path().join("right.sqlite3"), "cedar").unwrap());
    for store in [&left, &right] {
        store.bind_fleet(auth.fleet_id()).unwrap();
    }
    append(&left, "note/warmup");
    let partition = Arc::new(AtomicBool::new(false));
    let blocked = partition.clone();
    let heads = Arc::new(AtomicUsize::new(0));
    let head_count = heads.clone();
    let head_blocked = partition.clone();
    let posts = Arc::new(Mutex::new(Vec::new()));
    let recorded_posts = posts.clone();
    let state = PeerState::new(
        Local(right.clone()),
        "cedar".into(),
        auth.clone(),
        FleetContext::legacy(BTreeSet::from(["birch".into()])),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(
        axum::serve(
            listener,
            Router::new().route(
                EXCHANGE_PATH,
                axum::routing::head(move || {
                    head_count.fetch_add(1, Ordering::Relaxed);
                    let blocked = head_blocked.clone();
                    async move {
                        if blocked.load(Ordering::Relaxed) && !heads_alive_during_partition {
                            std::future::pending::<()>().await;
                        }
                        StatusCode::METHOD_NOT_ALLOWED
                    }
                })
                .post(move |headers: HeaderMap, bytes: Bytes| {
                    let state = state.clone();
                    let blocked = blocked.clone();
                    let posts = recorded_posts.clone();
                    async move {
                        posts.lock().unwrap().push(tokio::time::Instant::now());
                        // Both transports are black-holed until restoration. HEAD after
                        // restoration stays unauthenticated; only POST imports authority.
                        if blocked.load(Ordering::Relaxed) {
                            std::future::pending::<()>().await;
                        }
                        receive_exchange(State(state), headers, bytes).await
                    }
                }),
            ),
        )
        .into_future(),
    );
    let fleet = FleetContext::legacy(BTreeSet::from(["cedar".into()]));
    let (_route_tx, routes) = watch::channel(vec![Route::Http(url)]);
    let (notify, changes) = watch::channel(0);
    let phases = Arc::new(Mutex::new(Vec::new()));
    let dialer = tokio::spawn(dial_peer(
        ObservedWorker {
            local: Local(left.clone()),
            phases: phases.clone(),
        },
        "birch".into(),
        "cedar".into(),
        routes,
        auth,
        fleet,
        changes,
    ));
    let awake = tokio::spawn(async {
        loop {
            tokio::task::yield_now().await;
        }
    });
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let status = left
                .replication_status(false, None, &["cedar".into()])
                .unwrap();
            if !right
                .claims_for("note/warmup", Some("example.note"))
                .unwrap()
                .is_empty()
                && status.peers[0]
                    .worker
                    .as_ref()
                    .is_some_and(|w| w.phase == "idle")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("authenticated warmup completes before clock control");
    tokio::time::pause();
    partition.store(true, Ordering::Relaxed);
    append(&left, "note/pending");
    notify.send_modify(|v| *v += 1);
    let outage_start = tokio::time::Instant::now();
    let mut mature = false;
    for _ in 0..180 {
        for _ in 0..2000 {
            tokio::task::yield_now().await;
        }
        assert!(
            right
                .claims_for("note/pending", Some("example.note"))
                .unwrap()
                .is_empty()
        );
        let status = left
            .replication_status(false, None, &["cedar".into()])
            .unwrap();
        let worker = status.peers[0].worker.as_ref().unwrap();
        if outage_start.elapsed()
            >= Duration::from_secs(if heads_alive_during_partition {
                160
            } else {
                120
            })
            && (heads_alive_during_partition
                || (worker.phase == "backoff"
                    && worker
                        .next_retry_at_unix_ms
                        .is_some_and(|at| at > crate::store::now_ms() + 15_000)))
        {
            mature = true;
            break;
        }
        tokio::time::advance(Duration::from_secs(1)).await;
    }
    assert!(
        mature,
        "positive control reaches a mature retry window; negative observes at least 160 seconds"
    );
    if heads_alive_during_partition {
        let events = phases.lock().unwrap().clone();
        let consumed: Vec<_> = events
            .iter()
            .filter(|(at, phase, _)| *at >= outage_start && phase == "recovery-consumed")
            .collect();
        assert_eq!(
            consumed.len(),
            1,
            "repeated HEADs and failed POSTs must spend only once"
        );
        assert!(consumed[0].0.duration_since(outage_start) < Duration::from_secs(160));
        let original = events
            .iter()
            .rev()
            .find(|(at, phase, _)| *at < consumed[0].0 && phase == "backoff")
            .unwrap()
            .2
            .unwrap();
        assert_eq!(
            posts
                .lock()
                .unwrap()
                .iter()
                .filter(|at| **at >= consumed[0].0 && **at < original)
                .count(),
            1,
            "one early POST must actually reach the peer after spending credit"
        );
        let resumed_event = events
            .iter()
            .find(|(at, phase, _)| *at > consumed[0].0 && phase == "backoff")
            .unwrap();
        let resumed = resumed_event.2.unwrap();
        // Status publishes remaining time, so an already expired deadline is
        // represented as now. The paused controller advances in one-second ticks.
        assert!(resumed_event.0 <= original + Duration::from_secs(1));
        let expected = original.max(resumed_event.0);
        assert!(
            expected.max(resumed).duration_since(expected.min(resumed))
                < Duration::from_millis(100),
            "early failure keeps original remaining wait: original={:?}, resumed_at={:?}, reported={:?}",
            original.duration_since(outage_start),
            resumed_event.0.duration_since(outage_start),
            resumed.duration_since(outage_start)
        );
        assert!(
            events
                .iter()
                .any(|(at, phase, _)| *at > consumed[0].0 && phase == "recovery-refused-spent"),
            "expected later spent refusal; observed phases: {:?}",
            events
                .iter()
                .map(|(at, phase, _)| (at.duration_since(outage_start), phase))
                .collect::<Vec<_>>()
        );
        assert!(
            heads.load(Ordering::Relaxed) > 2,
            "repeated successful probes exercise spent-credit guard"
        );
        dialer.abort();
        awake.abort();
        server.abort();
        return;
    }
    let before = heads.load(Ordering::Relaxed);
    let clear = tokio::time::Instant::now();
    partition.store(false, Ordering::Relaxed);
    let mut delivered = false;
    for _ in 0..10 {
        for _ in 0..2000 {
            tokio::task::yield_now().await;
        }
        if !right
            .claims_for("note/pending", Some("example.note"))
            .unwrap()
            .is_empty()
        {
            delivered = true;
            break;
        }
        tokio::time::advance(Duration::from_secs(1)).await;
    }
    assert!(delivered && clear.elapsed() < Duration::from_secs(10));
    let events = phases.lock().unwrap().clone();
    assert_eq!(
        events
            .iter()
            .filter(|(at, phase, _)| *at >= clear && phase == "recovery-consumed")
            .count(),
        1,
        "the production recovery credit must be the path exercised, not an ordinary deadline expiry"
    );
    assert!(
        !events.iter().any(|(at, phase, _)| *at >= outage_start
            && *at < clear
            && phase == "recovery-consumed"),
        "black-holed probes cannot spend credit before restoration"
    );
    assert!(
        heads.load(Ordering::Relaxed) <= before + 10 / PEER_PROBE_INTERVAL.as_secs() as usize + 1
    );
    assert_eq!(
        right
            .claims_for("note/warmup", Some("example.note"))
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        right
            .claims_for("note/pending", Some("example.note"))
            .unwrap()
            .len(),
        1
    );
    dialer.abort();
    awake.abort();
    server.abort();
}

/// The legacy transport probe must still notice a late return after more than
/// ten failed probes. This helper control does not establish a historical schedule.
#[tokio::test(start_paused = true)]
async fn late_transport_return_preserves_probing_through_long_retry_windows() {
    for seconds in [64, 128] {
        let blocked = Arc::new(AtomicBool::new(true));
        let server_blocked = blocked.clone();
        let requests = Arc::new(AtomicUsize::new(0));
        let count = requests.clone();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(
            axum::serve(
                listener,
                Router::new().route(
                    EXCHANGE_PATH,
                    axum::routing::head(move || {
                        let blocked = server_blocked.clone();
                        let count = count.clone();
                        async move {
                            count.fetch_add(1, Ordering::Relaxed);
                            if blocked.load(Ordering::Relaxed) {
                                std::future::pending::<()>().await;
                            }
                            StatusCode::METHOD_NOT_ALLOWED
                        }
                    }),
                ),
            )
            .into_future(),
        );
        let awake = tokio::spawn(async {
            loop {
                tokio::task::yield_now().await;
            }
        });
        let backend = Local(Arc::new(plain_memory("birch").unwrap()));
        let fleet = FleetContext::legacy(BTreeSet::from(["cedar".into()]));
        let (_route_tx, mut routes) = watch::channel(vec![Route::Http(url.clone())]);
        let mut inbound = fleet.inbound_changed.subscribe();
        let mut activity = fleet.activity_changed.subscribe();
        let mut connectivity = fleet.connectivity_changed.subscribe();
        let http = replication_http_client();
        let started = tokio::time::Instant::now();
        let last_success = (url, started);
        let mut window = PeerRetryWindow::new(Duration::from_secs(seconds));
        let mut outcome = None;
        {
            let wait = wait_peer_retry_inner(
                &backend,
                crate::store::now_ms(),
                &mut window,
                &mut routes,
                &mut inbound,
                &mut activity,
                &mut connectivity,
                &fleet,
                "cedar",
                started,
                Some(&last_success),
                &http,
                None,
            );
            tokio::pin!(wait);
            for _ in 0..60 {
                if started.elapsed() >= Duration::from_secs(50) {
                    blocked.store(false, Ordering::Relaxed);
                }
                tokio::select! {
                    result = &mut wait => { outcome = Some(result); break; }
                    _ = async { for _ in 0..1000 { tokio::task::yield_now().await; } } => {}
                }
                tokio::time::advance(Duration::from_secs(1)).await;
            }
        }
        assert_eq!(outcome, Some(PeerRetryOutcome::Deadline));
        assert!(started.elapsed() >= Duration::from_secs(50));
        assert!(
            started.elapsed() <= Duration::from_secs(55),
            "eligible HEAD still detects late return, rather than waiting64/128s"
        );
        assert!(
            requests.load(Ordering::Relaxed) > 10,
            "control must exercise restoration after the removed ten-probe cutoff"
        );
        assert_eq!(window.deadline, started + Duration::from_secs(30));
        assert!(
            fleet.activity.read().unwrap().is_empty(),
            "HEAD405 is not authentication"
        );
        assert!(requests.load(Ordering::Relaxed) <= started.elapsed().as_secs() as usize / 3 + 1);
        awake.abort();
        server.abort();
    }
}
