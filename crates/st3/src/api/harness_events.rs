use super::*;
use crate::harness_events::Publication;

pub(super) async fn publish(
    State(state): State<AppState>,
    peer: Option<Extension<NativeDeliveryPeer>>,
    Json(request): Json<Publication>,
) -> Result<Json<ClaimRecord>, ApiError> {
    let Some(peer) = peer else {
        return Err(ApiError::bad(St3Error::new(
            "unbound-harness-event",
            "harness events require a local native driver",
        )));
    };
    if peer.agent != request.claim.subject
        || request.claim.actor.as_deref() != Some(&peer.agent)
        || !peer.archives_inbox && peer.transport != "pi-channel" && peer.transport != "omp-channel"
    {
        return Err(ApiError::bad(St3Error::new(
            "foreign-harness-event",
            "only this seat's native driver can publish its observations",
        )));
    }
    let store = state.store.clone();
    let kind = request.claim.kind.clone();
    let (record, changed, transition) =
        blocking_action(move || store.append_harness_event_publication(&request)).await?;
    finish_claim_publication(&state, &kind, record, changed, Some(transition)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{Body, to_bytes},
        http::Request,
    };

    pub(super) const SEAT: &str = "agent/example/native-wake";

    pub(super) fn runtime(state: &AppState, incarnation: &str) {
        state
            .store
            .append_claim(&ClaimInput {
                subject: SEAT.into(),
                kind: "runtime.observed".into(),
                actor: Some(SEAT.into()),
                fields: BTreeMap::from([
                    ("status".into(), json!("running")),
                    ("incarnation_id".into(), json!(incarnation)),
                ]),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
    }

    pub(super) fn event(sequence: u64, state: &str) -> Publication {
        Publication {
            runtime_incarnation: "native-one".into(),
            sequence,
            claim: ClaimInput {
                subject: SEAT.into(),
                kind: "harness.observed".into(),
                actor: Some(SEAT.into()),
                fields: BTreeMap::from([
                    ("state".into(), json!(state)),
                    ("driver".into(), json!("claude")),
                    ("incarnation_id".into(), json!("native-one")),
                ]),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: Some(format!("native-{sequence}")),
            },
        }
    }

    pub(super) async fn post(
        state: &AppState,
        event: &Publication,
        agent: &str,
    ) -> (StatusCode, Value) {
        let response = router(state.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/harness-events")
                    .header("content-type", "application/json")
                    .extension(NativeDeliveryPeer {
                        agent: agent.into(),
                        transport: "claude-channel",
                        pid: 37,
                        archives_inbox: true,
                    })
                    .body(Body::from(serde_json::to_vec(event).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let envelope: Value = serde_json::from_slice(&body).unwrap();
        let value = if status.is_success() {
            envelope["value"].clone()
        } else {
            envelope
        };
        (status, value)
    }

    pub(super) async fn notified(state: &AppState) -> bool {
        tokio::time::timeout(Duration::from_millis(20), state.notify.notified())
            .await
            .is_ok()
    }

    #[tokio::test]
    async fn native_readiness_and_idle_events_wake_reconcile_but_replay_does_not() {
        let root = tempfile::tempdir().unwrap();
        let state = super::super::tests::state(root.path());
        runtime(&state, "native-one");
        for (sequence, status) in [(1, "ready"), (2, "working"), (3, "idle"), (4, "blocked")] {
            let input = event(sequence, status);
            let (code, admitted) = post(&state, &input, SEAT).await;
            assert_eq!(code, StatusCode::OK, "{admitted}");
            assert!(
                admitted["id"]
                    .as_str()
                    .unwrap()
                    .starts_with("local-observation/")
            );
            assert!(
                notified(&state).await,
                "native {status} must wake reconcile"
            );
            assert_eq!(
                state.store.current_harness(SEAT).unwrap().unwrap().state,
                status
            );
            let (code, replay) = post(&state, &input, SEAT).await;
            assert_eq!(code, StatusCode::OK, "{replay}");
            assert_eq!(replay, admitted);
            assert!(!notified(&state).await, "exact replay stays quiet");
        }
    }

    #[tokio::test]
    async fn native_usage_and_rejected_or_stale_events_do_not_wake_reconcile() {
        let root = tempfile::tempdir().unwrap();
        let state = super::super::tests::state(root.path());
        runtime(&state, "native-one");
        let mut usage = event(1, "working");
        usage.claim.kind = "harness.usage".into();
        usage.claim.fields.remove("state");
        usage
            .claim
            .fields
            .insert("semantics".into(), json!("context_occupancy"));
        usage
            .claim
            .fields
            .insert("context_used_tokens".into(), json!(10));
        let (code, body) = post(&state, &usage, SEAT).await;
        assert_eq!(code, StatusCode::OK, "{body}");
        assert!(!notified(&state).await);
        let (code, body) = post(&state, &event(2, "idle"), "agent/example/other").await;
        assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert!(!notified(&state).await);
        runtime(&state, "native-two");
        let (code, body) = post(&state, &event(3, "ready"), SEAT).await;
        assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert_eq!(body["code"], "stale-harness-event-session");
        assert!(!notified(&state).await);
    }
}

#[cfg(test)]
mod heartbeat_tests {
    use super::tests::{SEAT, event, notified, post, runtime};
    use super::*;

    #[tokio::test]
    async fn native_same_state_heartbeats_do_not_wake_even_without_echoed_transition_metadata() {
        let root = tempfile::tempdir().unwrap();
        let state = super::super::tests::state(root.path());
        runtime(&state, "native-one");
        let (code, first) = post(&state, &event(1, "working"), SEAT).await;
        assert_eq!(code, StatusCode::OK, "{first}");
        assert!(notified(&state).await);
        for sequence in [2, 3] {
            let mut heartbeat = event(sequence, "working");
            if sequence == 3 {
                // A changed auxiliary reading publishes history, still with status_transition:false.
                heartbeat
                    .claim
                    .fields
                    .insert("background_jobs".into(), json!(1));
            }
            let (code, admitted) = post(&state, &heartbeat, SEAT).await;
            assert_eq!(code, StatusCode::OK, "{admitted}");
            assert!(
                admitted["body"]["fields"]
                    .get("status_transition")
                    .is_none(),
                "native admission echoes the producer fields unchanged"
            );
            assert!(
                !notified(&state).await,
                "same-state native heartbeat stays quiet"
            );
            let history = state
                .store
                .latest_claim(SEAT, Some("harness.observed"))
                .unwrap()
                .unwrap();
            assert_eq!(history.body["fields"]["status_transition"], sequence == 2);
        }
        let (code, idle) = post(&state, &event(4, "idle"), SEAT).await;
        assert_eq!(code, StatusCode::OK, "{idle}");
        assert!(notified(&state).await);
    }
}
