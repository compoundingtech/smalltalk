//! A self-requested restart waits for the driver's quiet boundary. These are existing runtime
//! action fields: older reconcilers ignore the pending action rather than interrupting a turn.

use crate::{model::ClaimRecord, store::Store};
use anyhow::Result;

pub(crate) const WHEN_IDLE: &str = "restart-when-idle";
pub(crate) const ATTENTION_AFTER_MS: u128 = 30 * 60 * 1_000;

pub(crate) fn latest_request(store: &Store, subject: &str) -> Result<Option<ClaimRecord>> {
    Ok(store
        .claims_for(subject, Some("runtime.action.requested"))?
        .into_iter()
        .rev()
        .find(|claim| {
            claim.actor.is_some()
                && matches!(
                    claim.body["fields"]["action"].as_str(),
                    Some("restart" | WHEN_IDLE)
                )
        }))
}

pub(crate) fn fresh_harness(
    store: &Store,
    subject: &str,
    incarnation: &str,
    now: u128,
) -> Result<Option<ClaimRecord>> {
    Ok(store
        .latest_observation(subject, "harness.observed")?
        .filter(|claim| {
            let fields = &claim.body["fields"];
            // Read driver evidence, never current_harness's work/progress overlay. Its acceptance
            // time cannot freshen an old report delivered late or replayed after a daemon restart.
            fields["incarnation_id"].as_str() == Some(incarnation)
                && fields["observed_at_ms"].as_u64().is_some_and(|at| {
                    let at = u128::from(at);
                    now.saturating_sub(at)
                        < st_drivers::harness_state::HARNESS_STATE_STALE.as_millis()
                        && at.saturating_sub(now)
                            <= st_drivers::harness_state::HARNESS_STATE_FUTURE_SKEW.as_millis()
                })
        }))
}

pub(crate) fn turn_in_flight(claim: &ClaimRecord) -> bool {
    claim.body["fields"]["state"] == "working"
        || claim.body["fields"]["blocking"]
            .as_array()
            .is_some_and(|blocking| blocking.iter().any(|reason| reason == "turn-in-flight"))
}

pub(crate) fn quiet(claim: &ClaimRecord) -> bool {
    let fields = &claim.body["fields"];
    fields["state"] == "idle"
        && fields["quiescent"] == true
        && fields["blocking"].as_array().is_some_and(Vec::is_empty)
        // Suspension also rejects an old OMP quiescence report that did not observe jobs.
        && (fields["driver"] != "omp" || fields["background_jobs"] == 0)
}

pub(crate) fn attention_episode(request: &ClaimRecord) -> String {
    format!("self-restart:{}", request.id)
}

pub(crate) fn release_key(request: &ClaimRecord) -> String {
    format!("agent-restart-quiesced:{}", request.id)
}
