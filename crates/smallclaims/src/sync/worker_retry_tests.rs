//! Focused controls for the private retry policy and its actual export/send callers.

use super::*;
use std::cell::Cell;
use std::error::Error as _;
use std::sync::atomic::Ordering;

const URL: &str = "http://127.0.0.1:31001";

fn fixture() -> (
    FleetContext,
    FleetAuth,
    watch::Sender<Vec<Route>>,
    watch::Receiver<Vec<Route>>,
) {
    let (tx, rx) = watch::channel(vec![Route::Http(URL.into())]);
    (
        FleetContext::legacy(BTreeSet::from(["beacon".into()])),
        FleetAuth::test("retry-control", &[42; 32]),
        tx,
        rx,
    )
}

fn earn(fleet: &FleetContext, routes: &watch::Receiver<Vec<Route>>) -> RecoveryCredit {
    let mut credit = RecoveryCredit::default();
    credit.earn(retry_generation(fleet, routes, URL), fleet, routes, URL);
    credit
}

fn schedule() -> RetrySchedule {
    RetrySchedule::new(tokio::time::Instant::now(), Duration::from_secs(3600))
}

#[tokio::test(start_paused = true)]
async fn credit_starts_absent_expires_and_is_spent_once() {
    let (fleet, auth, _tx, routes) = fixture();
    let mut credit = RecoveryCredit::default();
    assert!(
        credit
            .spend(&fleet, &auth, &routes, URL, schedule())
            .is_none()
    );
    credit = earn(&fleet, &routes);
    let mut ordinary = schedule();
    ordinary.transport_alive();
    let first = credit.spend(&fleet, &auth, &routes, URL, ordinary).unwrap();
    assert_eq!(
        first.schedule.deadline,
        ordinary.started + Duration::from_secs(30)
    );
    drop(first); // cancellation/drop cannot restore a consumed value
    for _ in 0..10 {
        ordinary.transport_alive();
        fleet.note_activity("beacon");
        assert!(
            credit
                .spend(&fleet, &auth, &routes, URL, ordinary)
                .is_none()
        );
    }
    let mut another_peer = RecoveryCredit::default();
    assert!(
        another_peer
            .spend(&fleet, &auth, &routes, URL, ordinary)
            .is_none()
    );
    credit = earn(&fleet, &routes); // only the successful-exchange caller may do this
    tokio::time::advance(PEER_PROBE_WINDOW).await;
    assert!(
        credit
            .spend(&fleet, &auth, &routes, URL, schedule())
            .is_none()
    );
    let mut restarted = RecoveryCredit::default();
    assert!(
        restarted
            .spend(&fleet, &auth, &routes, URL, schedule())
            .is_none()
    );
}

#[tokio::test(start_paused = true)]
async fn wake_caller_and_failed_advance_keep_the_original_counter_and_cap() {
    let (fleet, auth, _tx, routes) = fixture();
    let mut credit = earn(&fleet, &routes);
    let mut backoff = PeerBackoff { failures: 10 };
    let ordinary_plan = failed_retry_plan(&mut backoff, None, |backoff| backoff.next());
    tokio::time::advance(Duration::from_secs(1)).await; // visibility precedes the ordinary wait
    let mut ordinary = ordinary_plan.after_visibility(tokio::time::Instant::now());
    assert_eq!(backoff.failures, 11);
    ordinary.transport_alive();
    let advance = credit.spend(&fleet, &auth, &routes, URL, ordinary).unwrap();
    let advance = PeerRetryWake::Advance(advance)
        .into_advance(&mut backoff, &mut credit)
        .unwrap();
    assert_eq!(
        backoff.failures, 11,
        "HEAD wake is not an authenticated reset"
    );
    tokio::time::advance(Duration::from_secs(20)).await; // slow export or failed submission
    let resumed = failed_retry_plan(&mut backoff, Some(advance.schedule), |_| {
        panic!("advanced failure took another backoff step")
    })
    .after_visibility(tokio::time::Instant::now());
    assert_eq!(resumed, ordinary);
    let mut resumed_again = resumed;
    resumed_again.transport_alive();
    assert_eq!(
        resumed_again.deadline, ordinary.deadline,
        "cap restarted after the advance"
    );
    tokio::time::advance(Duration::from_secs(11)).await;
    assert!(
        resumed_again.remaining().is_zero(),
        "already-due deadline was restarted"
    );
    assert!(
        credit
            .spend(&fleet, &auth, &routes, URL, resumed_again)
            .is_none()
    );
    assert!(
        PeerRetryWake::Deadline
            .into_advance(&mut backoff, &mut credit)
            .is_none()
    );
    assert_eq!(backoff.failures, 11);
    assert!(
        PeerRetryWake::Changed
            .into_advance(&mut backoff, &mut credit)
            .is_none()
    );
    assert_eq!(
        backoff.failures, 0,
        "existing authenticated/route wake still resets backoff"
    );
    assert!(credit.earned.is_none());
}

#[tokio::test(start_paused = true)]
async fn already_due_and_new_signed_cooldown_do_not_allow_an_advance() {
    let (fleet, auth, _tx, routes) = fixture();
    let mut credit = earn(&fleet, &routes);
    let due = RetrySchedule::new(tokio::time::Instant::now(), Duration::ZERO);
    assert!(credit.spend(&fleet, &auth, &routes, URL, due).is_none());
    let mut ordinary = schedule();
    ordinary.transport_alive();
    tokio::time::advance(Duration::from_secs(29)).await;
    let cooldown = tokio::time::Instant::now() + Duration::from_secs(30);
    ordinary.honor_cooldown(cooldown);
    for _ in 0..10 {
        ordinary.transport_alive();
    }
    assert_eq!(
        ordinary.deadline, cooldown,
        "HEAD bypassed a new signed cooldown"
    );
    assert!(!PeerHttpFailure::can_advance(
        &PeerOverloaded {
            retry_after: Duration::from_secs(30)
        }
        .into()
    ));
    let (_fabric_tx, fabric_rx) = watch::channel(vec![Route::Fabric {
        node: "beacon".into(),
        protocol: "invented-protocol".into(),
    }]);
    assert!(retry_generation(&fleet, &fabric_rx, URL).is_none());
    assert!(
        credit
            .spend(&fleet, &auth, &fabric_rx, URL, ordinary)
            .is_none()
    );
    for jitter in [0, 200, 400, u16::MAX] {
        assert!(fabric_refusal_delay_with_jitter(jitter) >= Duration::from_secs(1440));
    }
}

#[tokio::test(start_paused = true)]
async fn earn_spend_and_submission_reject_changed_authority_routes_and_auth() {
    for change in 0..4 {
        let (fleet, auth, tx, routes) = fixture();
        let mut credit = earn(&fleet, &routes);
        let advance = credit
            .spend(&fleet, &auth, &routes, URL, schedule())
            .unwrap();
        let mut used_auth = auth.clone();
        match change {
            0 => {
                let _view = fleet.view.write().unwrap();
                fleet
                    .view_changed
                    .send_modify(|generation| *generation += 1);
            }
            1 => {
                tx.send_replace(vec![Route::Http("http://127.0.0.1:31002".into())]);
            }
            2 => fleet.removed.store(true, Ordering::Release),
            _ => used_auth = FleetAuth::test("retry-control", &[42; 32]),
        }
        assert!(advance.check(&fleet, &used_auth, &routes, URL).is_err());
        let exports = Cell::new(0);
        assert!(
            retry_export(
                Some(RetrySubmission {
                    advance: &advance,
                    routes: &routes
                }),
                &fleet,
                &used_auth,
                URL,
                || async {
                    exports.set(exports.get() + 1);
                    Ok(())
                }
            )
            .await
            .is_err()
        );
        assert_eq!(exports.get(), 0, "invalid permission started an export");
        assert!(
            credit
                .spend(&fleet, &auth, &routes, URL, schedule())
                .is_none()
        );
    }
    let (fleet, auth, tx, mut routes) = fixture();
    let other = "http://127.0.0.1:31002";
    tx.send_replace(vec![Route::Http(URL.into()), Route::Http(other.into())]);
    routes.borrow_and_update();
    let mut credit = earn(&fleet, &routes);
    assert!(
        credit
            .spend(&fleet, &auth, &routes, other, schedule())
            .is_none(),
        "a different available route borrowed the successful route's credit"
    );
    let mut credit = RecoveryCredit::default();
    credit.earn(Some(1), &fleet, &routes, URL); // generation changed across the exchange
    assert!(
        credit
            .spend(&fleet, &auth, &routes, URL, schedule())
            .is_none()
    );
}

#[tokio::test(start_paused = true)]
async fn actual_export_and_submission_callers_dispatch_once_and_fence_slow_export() {
    for change_after_export in 0..5 {
        let (fleet, auth, tx, routes) = fixture();
        let mut credit = earn(&fleet, &routes);
        let advance = credit
            .spend(&fleet, &auth, &routes, URL, schedule())
            .unwrap();
        let submission = Some(RetrySubmission {
            advance: &advance,
            routes: &routes,
        });
        let exports = Cell::new(0);
        let sends = Cell::new(0);
        let value = retry_export(submission, &fleet, &auth, URL, || async {
            exports.set(exports.get() + 1);
            match change_after_export {
                1 => {
                    let _view = fleet.view.write().unwrap();
                    fleet
                        .view_changed
                        .send_modify(|generation| *generation += 1);
                }
                2 => {
                    tx.send_replace(vec![Route::Http("http://127.0.0.1:31002".into())]);
                }
                3 => fleet.removed.store(true, Ordering::Release),
                4 => tokio::time::advance(PEER_PROBE_WINDOW).await,
                _ => tokio::time::advance(Duration::from_secs(20)).await,
            }
            Ok(17)
        })
        .await;
        if let Ok(value) = value {
            assert_eq!(change_after_export, 0);
            assert_eq!(
                retry_submit(submission, &fleet, &auth, URL, || async {
                    sends.set(sends.get() + 1);
                    Ok(value)
                })
                .await
                .unwrap(),
                17
            );
        }
        assert_eq!(exports.get(), 1);
        assert_eq!(sends.get(), usize::from(change_after_export == 0));
        assert!(
            credit
                .spend(&fleet, &auth, &routes, URL, schedule())
                .is_none()
        );
    }
}

#[tokio::test(start_paused = true)]
async fn export_outlasting_the_original_cap_does_not_restart_failure_wait() {
    let (fleet, auth, _tx, routes) = fixture();
    let mut credit = earn(&fleet, &routes);
    let advance = credit
        .spend(&fleet, &auth, &routes, URL, schedule())
        .unwrap();
    let retained = advance.schedule;
    assert_eq!(
        retained.deadline,
        retained.started + Duration::from_secs(30)
    );
    let submission = Some(RetrySubmission {
        advance: &advance,
        routes: &routes,
    });
    retry_export(submission, &fleet, &auth, URL, || async {
        tokio::time::advance(Duration::from_secs(31)).await;
        Ok(())
    })
    .await
    .unwrap();
    // It is now the ordinary deadline too; permit one call, then resume the overdue
    // absolute schedule on failure rather than adding a second delay from this time.
    let sends = Cell::new(0);
    assert!(
        retry_submit(submission, &fleet, &auth, URL, || async {
            sends.set(sends.get() + 1);
            Err::<(), _>(anyhow::anyhow!("exported, then submission failed"))
        })
        .await
        .is_err()
    );
    assert_eq!(sends.get(), 1);
    let mut backoff = PeerBackoff { failures: 11 };
    let resumed = failed_retry_plan(&mut backoff, Some(retained), |_| panic!("new backoff step"))
        .after_visibility(tokio::time::Instant::now());
    assert_eq!(resumed, retained);
    assert!(resumed.remaining().is_zero());
    assert_eq!(backoff.failures, 11);
    assert!(
        credit
            .spend(&fleet, &auth, &routes, URL, schedule())
            .is_none()
    );
}

#[tokio::test(start_paused = true)]
async fn submission_checks_changes_after_export_and_cancellation_never_rearms() {
    use futures_util::FutureExt as _;
    let (fleet, auth, tx, routes) = fixture();
    let mut credit = earn(&fleet, &routes);
    let advance = credit
        .spend(&fleet, &auth, &routes, URL, schedule())
        .unwrap();
    let submission = Some(RetrySubmission {
        advance: &advance,
        routes: &routes,
    });
    retry_export(submission, &fleet, &auth, URL, || async { Ok(()) })
        .await
        .unwrap();
    tx.send_replace(vec![Route::Http("http://127.0.0.1:31002".into())]);
    let sends = Cell::new(0);
    assert!(
        retry_submit(submission, &fleet, &auth, URL, || async {
            sends.set(sends.get() + 1);
            Ok(())
        })
        .await
        .is_err()
    );
    assert_eq!(sends.get(), 0);

    let (fleet, auth, _tx, routes) = fixture();
    credit = earn(&fleet, &routes);
    let advance = credit
        .spend(&fleet, &auth, &routes, URL, schedule())
        .unwrap();
    let submission = Some(RetrySubmission {
        advance: &advance,
        routes: &routes,
    });
    let exports = Cell::new(0);
    let pending = retry_export(submission, &fleet, &auth, URL, || async {
        exports.set(exports.get() + 1);
        std::future::pending::<Result<()>>().await
    });
    assert!(pending.now_or_never().is_none()); // poll once, then cancel/drop that owned future
    assert_eq!(exports.get(), 1);
    drop(advance);
    assert!(
        credit
            .spend(&fleet, &auth, &routes, URL, schedule())
            .is_none()
    );
}

#[test]
fn typed_status_builder_and_non_http_failures_are_not_transport_permissions() {
    let status = reqwest::Response::from(
        axum::http::Response::builder()
            .status(401)
            .body("not authenticated".to_string())
            .unwrap(),
    )
    .error_for_status()
    .unwrap_err()
    .with_url(URL.parse().unwrap());
    let error: anyhow::Error = PeerHttpFailure {
        phase: HttpFailurePhase::BeforeHeaders,
        cause: status,
    }
    .into();
    assert!(!PeerHttpFailure::can_advance(&error));
    assert!(
        error
            .downcast_ref::<PeerHttpFailure>()
            .unwrap()
            .source()
            .unwrap()
            .is::<reqwest::Error>()
    );
    let builder = reqwest::Client::new()
        .get("not a url")
        .build()
        .unwrap_err()
        .with_url(URL.parse().unwrap());
    let error: anyhow::Error = PeerHttpFailure {
        phase: HttpFailurePhase::BeforeHeaders,
        cause: builder,
    }
    .into();
    assert!(!PeerHttpFailure::can_advance(&error));
    for error in [
        anyhow::anyhow!("signature refusal"),
        anyhow::anyhow!("export failure"),
        anyhow::anyhow!("decode/version failure"),
    ] {
        assert!(!PeerHttpFailure::can_advance(&error));
    }
}

// The listener never supplies headers. A zero request timeout creates a real reqwest
// request error without a response deadline race, sleeps or guessed socket yields.
async fn timeout_cause() -> reqwest::Error {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let error = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::ZERO)
        .build()
        .unwrap()
        .post(format!(
            "http://{}/v1/peer/exchange",
            listener.local_addr().unwrap()
        ))
        .body("signed-request-fixture")
        .send()
        .await
        .unwrap_err();
    assert!(error.is_timeout());
    error
}

#[tokio::test]
async fn structural_reqwest_timeout_is_phase_and_scheme_specific() {
    let cause = timeout_cause().await;
    let error: anyhow::Error = PeerHttpFailure {
        phase: HttpFailurePhase::BeforeHeaders,
        cause,
    }
    .into();
    assert!(PeerHttpFailure::can_advance(&error));
    assert!(
        error
            .downcast_ref::<PeerHttpFailure>()
            .unwrap()
            .source()
            .unwrap()
            .is::<reqwest::Error>()
    );
    let cause = timeout_cause().await;
    let error: anyhow::Error = PeerHttpFailure {
        phase: HttpFailurePhase::ResponseBody,
        cause,
    }
    .into();
    assert!(
        !PeerHttpFailure::can_advance(&error),
        "same timeout after headers is not eligible"
    );
    let cause = timeout_cause()
        .await
        .with_url("https://127.0.0.1:31001".parse().unwrap());
    let error: anyhow::Error = PeerHttpFailure {
        phase: HttpFailurePhase::BeforeHeaders,
        cause,
    }
    .into();
    assert!(
        !PeerHttpFailure::can_advance(&error),
        "HTTPS metadata must refuse this conservative policy"
    );
}

#[tokio::test]
async fn lost_headers_do_not_undo_remote_effect_and_signed_duplicate_is_idempotent() {
    use crate::claim::ClaimInput;
    use crate::store::Store;
    use crate::store::runtime::Plain;
    use crate::sync::Local;
    let (fleet, auth, _tx, _routes) = fixture();
    let left = Store::open_memory("birch", Arc::new(Plain)).unwrap();
    let right = Store::open_memory("beacon", Arc::new(Plain)).unwrap();
    left.bind_fleet(auth.fleet_id()).unwrap();
    right.bind_fleet(auth.fleet_id()).unwrap();
    left.append_claim(&ClaimInput {
        subject: "note/ambiguous-response".into(),
        kind: "example.note".into(),
        actor: None,
        fields: BTreeMap::from([("text".into(), Value::String("retained once".into()))]),
        evidence: Vec::new(),
        expected_subject: None,
        idempotency_key: None,
    })
    .unwrap();
    let request = left
        .export_replication_exchange(auth.fleet_id(), &ReplicationInventory::default())
        .unwrap();
    let body = serde_json::to_vec(&request).unwrap();
    let headers = auth
        .request_headers_for(EXCHANGE_PATH, "birch", &body)
        .unwrap();
    let state = PeerState {
        backend: Local(Arc::new(right)),
        node: "beacon".into(),
        auth: auth.clone(),
        fleet: FleetContext::legacy(BTreeSet::from(["birch".into()])),
        outbound_notify: watch::channel(0).0,
    };
    let cause = timeout_cause().await;
    // This is an explicit lost-response seam, not a claim that this fixture produced a
    // socket timeout after remote acceptance. Run the real admission/receive first, then
    // give the real submission caller a typed no-headers error while withholding its answer.
    let error = retry_submit(None, &fleet, &auth, URL, || async {
        let answer = receive_exchange(
            State(state.clone()),
            headers.clone(),
            Bytes::from(body.clone()),
        )
        .await;
        assert!(answer.status().is_success());
        Err::<(), anyhow::Error>(
            PeerHttpFailure {
                phase: HttpFailurePhase::BeforeHeaders,
                cause,
            }
            .into(),
        )
    })
    .await
    .unwrap_err();
    assert!(PeerHttpFailure::can_advance(&error));
    assert_eq!(
        state
            .backend
            .0
            .claims_for("note/ambiguous-response", Some("example.note"))
            .unwrap()
            .len(),
        1
    );
    let answer = receive_exchange(State(state.clone()), headers, Bytes::from(body.clone())).await;
    assert!(answer.status().is_success());
    let answer_headers = answer.headers().clone();
    let answer_body = axum::body::to_bytes(answer.into_body(), MAX_EXCHANGE_BYTES)
        .await
        .unwrap();
    auth.verify_sender(
        &answer_headers,
        "RESPONSE",
        EXCHANGE_PATH,
        &answer_body,
        Some("beacon"),
        Some(&FleetAuth::body_digest(&body)),
    )
    .unwrap();
    assert_eq!(
        state
            .backend
            .0
            .claims_for("note/ambiguous-response", Some("example.note"))
            .unwrap()
            .len(),
        1
    );
}
