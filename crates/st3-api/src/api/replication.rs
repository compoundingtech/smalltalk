use super::*;

pub async fn replication_status(
    State(state): State<AppState>,
) -> Result<Json<ReplicationStatus>, ApiError> {
    let store = state.store.clone();
    let configured = state.fleet_id.is_some();
    let fleet = state.fleet_id.clone();
    let peers = replication_peer_names(&state);
    let state_dir = state.state_dir.clone();
    blocking_store(move || {
        let mut status = store.replication_status_sealed(configured, fleet.as_deref(), &peers)?;
        status.removed = crate::fleet::file::RemovalNotice::load(&state_dir);
        Ok(status)
    })
    .await
    .map(Json)
}

#[derive(Deserialize)]
pub struct ReplicationRecordsQuery {
    #[serde(default = "default_true")]
    unresolved: bool,
}

pub async fn replication_records(
    State(state): State<AppState>,
    Query(query): Query<ReplicationRecordsQuery>,
) -> Result<Json<Vec<ReplicaRecordView>>, ApiError> {
    let store = state.store.clone();
    blocking_store(move || store.replica_records(query.unresolved))
        .await
        .map(Json)
}

pub async fn replication_record(
    State(state): State<AppState>,
    AxumPath(record): AxumPath<String>,
) -> Result<Json<ReplicaRecordView>, ApiError> {
    let record = if record.starts_with("record/") {
        record
    } else {
        format!("record/{record}")
    };
    let store = state.store.clone();
    let record_for_read = record.clone();
    blocking_store(move || store.replica_record(&record_for_read))
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("replica record `{record}` does not exist")))
}

pub async fn checkpoint_plan(
    State(state): State<AppState>,
    Json(request): Json<crate::store::CheckpointPlanRequest>,
) -> Result<Json<crate::store::CheckpointPlanView>, ApiError> {
    let cut = match request.day.as_deref() {
        Some(day) => crate::store::checkpoint_cut(day).map_err(ApiError::bad)?,
        None => crate::store::newest_due_cut(client_now_ms()),
    };
    let scratch = state.state_dir.join("checkpoint");
    let capture_dir = scratch.join("plan");
    let capture = state.store.clone();
    let staged = blocking_store(move || capture.checkpoint_staged_set(cut, None, &capture_dir)).await?;
    let store = state.store.clone();
    // The capture may seal/register the live writer. Planning and proof on the
    // scratch copy do not borrow it and must not occupy its worker capacity.
    blocking_external(move || store.checkpoint_plan_view_from_staged(cut, staged, &scratch))
        .await?.map(Json).map_err(ApiError::internal)
}

pub async fn checkpoint_status(
    State(state): State<AppState>,
) -> Result<Json<crate::store::CheckpointStatusView>, ApiError> {
    let store = state.store.clone();
    let peers = state.configured_peers.clone();
    blocking_store(move || store.checkpoint_status(client_now_ms(), &peers))
        .await
        .map(Json)
}

pub async fn checkpoint_excuse(
    State(state): State<AppState>,
    Json(request): Json<crate::store::CheckpointExcuseRequest>,
) -> Result<Json<ClaimRecord>, ApiError> {
    let claim = state
        .store
        .excuse_checkpoint_writer(&request)
        .map_err(ApiError::bad)?;
    signal_changed(&state);
    Ok(Json(claim))
}

pub async fn checkpoint_resume(
    State(state): State<AppState>,
    Json(request): Json<crate::store::CheckpointResumeRequest>,
) -> Result<Json<crate::store::CheckpointStatusView>, ApiError> {
    let store = state.store.clone();
    blocking_action(move || store.resume_checkpoints(&request.actor, &request.reason)).await?;
    signal_changed(&state);
    let store = state.store.clone();
    let peers = state.configured_peers.clone();
    blocking_store(move || store.checkpoint_status(client_now_ms(), &peers))
        .await
        .map(Json)
}

pub async fn repair_replication_record(
    State(state): State<AppState>,
    Json(request): Json<ReplicationRepairRequest>,
) -> Result<Json<ClaimRecord>, ApiError> {
    let claim = state
        .store
        .repair_replica_record(
            &request.record_ref,
            &request.replacement_claim_id,
            &request.reason,
            &request.actor,
            &request.idempotency_key,
        )
        .map_err(ApiError::bad)?;
    signal_changed(&state);
    Ok(Json(claim))
}

pub async fn replication_export(
    State(state): State<AppState>,
    Json(request): Json<ReplicationExportRequest>,
) -> Result<Json<ReplicationExportResponse>, ApiError> {
    #[cfg(feature = "test-support")]
    if !request.summary_only
        && let Ok(millis) = std::env::var("ST3_TEST_REPLICATION_EXPORT_DELAY_MS")
        && let Ok(millis) = millis.parse::<u64>()
    {
        tokio::time::sleep(Duration::from_millis(millis)).await;
    }
    let store = state.store.clone();
    blocking_store(move || {
        let exchange = if request.summary_only {
            record_replication_summary(request.authority_guard, || {
                store.export_replication_summary(&request.fleet_id)
            })?
        } else {
            store.export_replication_exchange_answering(
                &request.fleet_id,
                &request.inventory,
                &request.signature_requests,
            )?
        };
        Ok(Json(ReplicationExportResponse {
            exchange,
            store_index: store.index()?,
        }))
    })
    .await
}

/// One page of a checkpoint's manifest, for the replication worker to answer a peer with.
pub async fn replication_checkpoint_manifest(
    State(state): State<AppState>,
    Json(request): Json<crate::store::CheckpointManifestRequest>,
) -> Result<Json<crate::store::CheckpointManifestPage>, ApiError> {
    let store = state.store.clone();
    blocking_store(move || store.checkpoint_manifest_page(&request))
        .await
        .map(Json)
}

/// The newest stable checkpoint this node needs a peer's manifest for, if any.
pub async fn replication_checkpoint_need(
    State(state): State<AppState>,
) -> Result<Json<Option<crate::store::CheckpointManifestNeed>>, ApiError> {
    let store = state.store.clone();
    blocking_store(move || store.checkpoint_manifest_need())
        .await
        .map(Json)
}

/// Adopt a checkpoint from the manifest the replication worker fetched from a peer.
pub async fn replication_checkpoint_adopt(
    State(state): State<AppState>,
    Json(manifest): Json<crate::store::CheckpointManifest>,
) -> Result<Json<Vec<crate::store::CheckpointAction>>, ApiError> {
    let store = state.store.clone();
    let actions = blocking_action(move || store.adopt_checkpoint(&manifest)).await?;
    if !actions.is_empty() {
        signal_changed(&state);
    }
    Ok(Json(actions))
}

pub async fn replication_receive(
    State(state): State<AppState>,
    Json(request): Json<ReplicationReceiveRequest>,
) -> Result<Json<ReplicationReceiveResponse>, ApiError> {
    let store = state.store.clone();
    let (response, reconcile_changed, notice_visible) = blocking_action(move || {
        let before_index = store
            .index()
            .map_err(|error| St3Error::new("internal", error.to_string()))?;
        let projection_was_healthy = !store
            .replication_projection_needs_recovery()
            .map_err(|error| St3Error::new("internal", error.to_string()))?;
        if let Some(round_trip_ms) = request.round_trip_ms {
            store.record_replication_round_trip(Duration::from_millis(round_trip_ms));
        }
        let notice_was_visible = store.replication_peer_in_sync_notice(&request.peer);
        // Only the worker's own requests carry a round trip, and only they prove its route out.
        let receipt = store.receive_replication_exchange_via(
            &request.peer,
            &request.fleet_id,
            &request.exchange,
            request.round_trip_ms.is_some(),
            request.carrier,
        )?;
        if store
            .observes_transport_to(&request.peer)
            .map_err(|error| St3Error::new("internal", error.to_string()))?
        {
            store
                .record_transport_observation(&request.peer, "up", None, None)
                .map_err(|error| St3Error::new("internal", error.to_string()))?;
        }
        let new_data = replication_receive_has_new_data(receipt.received + receipt.signatures);
        let (admission, repairs) = if new_data {
            let admission = store
                .validate_replication_backlog()
                .map_err(|error| St3Error::new("internal", error.to_string()))?;
            let repairs = store
                .apply_replication_repairs()
                .map_err(|error| St3Error::new("internal", error.to_string()))?;
            (admission, repairs)
        } else {
            (Default::default(), 0)
        };
        // A catching-up node defers projection; the first receive after the deferral window,
        // with or without new data, projects what it admitted meanwhile.
        let was_deferred = store.replication_projection_deferred();
        let projection = if new_data || was_deferred {
            use opentelemetry::trace::{Span as _, TraceContextExt as _};
            use tracing_opentelemetry::OpenTelemetrySpanExt as _;
            let receive = crate::otel::export_enabled().then(|| {
                tracing::Span::current().context().span().span_context().clone()
            });
            let mut span = crate::otel::stage_root(
                "st.replication.projection", "receive", receive,
            );
            let result = admitted_projection_result(
                store.project_replication_backlog_unless_catching_up(),
            );
            if let Some(span) = span.as_mut() {
                span.set_attribute(opentelemetry::KeyValue::new(
                    "st.replication.peer", request.peer.clone(),
                ));
                span.set_attribute(opentelemetry::KeyValue::new(
                    "st.replication.moved_envelopes", receipt.received as i64,
                ));
                span.set_attribute(opentelemetry::KeyValue::new(
                    "st.replication.outcome",
                    match &result {
                        Ok(Some(true)) => "projected",
                        Ok(Some(false)) => "unchanged",
                        Ok(None) => "deferred",
                        Err(_) => "error",
                    },
                ));
                if result.is_err() {
                    span.set_status(opentelemetry::trace::Status::error("projection failed"));
                }
                span.end();
            }
            result?
        } else {
            Some(true)
        };
        let projected = projection.unwrap_or(false);
        let store_index = store
            .index()
            .map_err(|error| St3Error::new("internal", error.to_string()))?;
        let changed = store_index != before_index
            || (projected && (admission.changed || repairs != 0 || was_deferred));
        let quiet_only = projection_was_healthy
            && projected
            && repairs == 0
            && admission.unknown == 0
            && admission.invalid == 0
            && store
                .claims_since_only_quiet_notifications(before_index)
                .map_err(|error| St3Error::new("internal", error.to_string()))?;
        // A deferred projection left the graph as it was, so the reconciler has nothing new.
        Ok((
            ReplicationReceiveResponse {
                receipt,
                changed,
                store_index,
            },
            changed && !quiet_only && projected,
            notice_was_visible || store.replication_peer_in_sync_notice(&request.peer),
        ))
    })
    .await?;
    if response.changed && reconcile_changed {
        crate::reconcile_telemetry::record_wake(
            crate::reconcile_telemetry::WakeCause::ReplicationReceive,
            None,
        );
        state.notify.notify_one();
    }
    // Outside the sync notice set, local carrier evidence has no client representation.
    // Quiet exchanges must not wake every client stream merely to advance a timestamp.
    if response.changed || notice_visible {
        state
            .event_notify
            .send_modify(|generation| *generation = generation.saturating_add(1));
    }
    Ok(Json(response))
}

pub async fn replication_peer_failure(
    State(state): State<AppState>,
    Json(request): Json<ReplicationPeerFailureRequest>,
) -> Result<Json<Value>, ApiError> {
    let store = state.store.clone();
    let (changed, visible_carrier_changed) = blocking_store(move || {
        let before_index = store.index()?;
        let previous = store.replication_peer_transport(&request.peer).1;
        let stale = store.record_peer_failure_via(
            &request.peer,
            &request.status,
            &request.error,
            request.carrier,
        )?;
        if stale && store.observes_transport_to(&request.peer)? {
            store.record_transport_observation(
                &request.peer,
                &request.status,
                Some(&request.error),
                None,
            )?;
        }
        // A peer that fails mid-sync sends no more exchanges, so project what it delivered.
        let projected = store.replication_projection_deferred()
            && store.project_replication_backlog_unless_catching_up()? == Some(true);
        let current = store.replication_peer_transport(&request.peer).1;
        // Repeated failures on the same carrier retain a fresh local timestamp without
        // waking all clients. First evidence or a carrier change matters only in a notice.
        let carrier = |record: &smallclaims::replication::ReplicationExchangeTransport| {
            (record.carrier, record.fabric_route.clone())
        };
        let visible_carrier_changed = previous.as_ref().map(carrier)
            != current.as_ref().map(carrier)
            && store.replication_peer_in_sync_notice(&request.peer);
        Ok((
            store.index()? != before_index || projected,
            visible_carrier_changed,
        ))
    })
    .await?;
    if changed {
        signal_changed(&state);
    } else if visible_carrier_changed {
        state
            .event_notify
            .send_modify(|generation| *generation = generation.saturating_add(1));
    }
    Ok(Json(json!({ "recorded": true, "changed": changed })))
}

pub async fn replication_wake(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let store = state.store.clone();
    let (admission, repairs, projection_attempted, projected, was_deferred) =
        blocking_store(move || {
            let admission = store.validate_replication_backlog()?;
            let repairs = store.apply_replication_repairs()?;
            let was_deferred = store.replication_projection_deferred();
            let projection_attempted = admission.changed
                || repairs != 0
                || was_deferred
                || store.replication_projection_needs_recovery()?;
            let projected = if projection_attempted {
                store
                    .project_replication_backlog_unless_catching_up()?
                    .unwrap_or(false)
            } else {
                true
            };
            Ok((
                admission,
                repairs,
                projection_attempted,
                projected,
                was_deferred,
            ))
        })
        .await?;
    if projected && (admission.changed || repairs != 0 || was_deferred) {
        signal_changed(&state);
    }
    Ok(Json(json!({
        "admitted": admission.valid,
        "unknown": admission.unknown,
        "invalid": admission.invalid,
        "repairs": repairs,
        "projected": projected,
        "projection_attempted": projection_attempted,
    })))
}
