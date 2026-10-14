//! Missing-channel recovery uses the ordinary fenced restart path and its native-session
//! continuation. Actor-bound receipts keep the retry budget and parking durable across restarts
//! and checkpoints. No message or delivery receipt is changed by recovery.
use super::*;
use serde_json::json;

const MAX_ATTEMPTS: usize = 3;
const RECHECK_BACKOFF_MS: u128 = 10_000;
const RECOVERY: &str = "claude-channel-recovery";

impl<R: RuntimeControl> Reconciler<R> {
    /// The driver rechecks attachment every second. After its detection window, allow another
    /// backoff window for a late MCP initialization before replacing the exact blocked runtime.
    pub(super) fn reconcile_claude_channel_recovery(
        &self,
        subject: &DesiredSubject,
        member: &MemberSpec,
        observation: Option<&RuntimeObservation>,
        blocked: Option<&anyhow::Error>,
        now: u128,
    ) -> Result<bool> {
        if subject.kind != "agent"
            || member.driver.as_deref() != Some("claude")
            || member.lifecycle != MemberLifecycle::Service
        {
            return Ok(false);
        }
        let Some(token) = self.store.selected_desired_token(&subject.subject)? else {
            return Ok(false);
        };
        let requests = self
            .store
            .claims_for(&subject.subject, Some("runtime.action.requested"))?;
        let restarts: Vec<_> = requests
            .iter()
            .filter(|claim| {
                claim.actor.is_some()
                    && claim.body["fields"]["action"] == "restart"
                    && claim.body.pointer("/evidence/0").and_then(Value::as_str) == Some(&token)
            })
            .collect();
        // An operator restart or a verified successful replacement starts a new recovery budget.
        // A late attachment claim from an old incarnation cannot reset it.
        let successes = self
            .store
            .claims_for(&subject.subject, Some("runtime.action.succeeded"))?;
        let reset = restarts
            .iter()
            .filter(|claim| claim.body["fields"]["operation"] != RECOVERY)
            .map(|claim| claim.store_index)
            .chain(
                successes
                    .iter()
                    .filter(|claim| {
                        claim.body["fields"]["action"] == "restart"
                            && restarts.iter().any(|request| {
                                claim.body.pointer("/evidence/0").and_then(Value::as_str)
                                    == Some(&request.id)
                            })
                    })
                    .map(|claim| claim.store_index),
            )
            .max()
            .unwrap_or(0);
        let attempts: Vec<_> = restarts
            .iter()
            .filter(|claim| {
                claim.store_index > reset && claim.body["fields"]["operation"] == RECOVERY
            })
            .collect();
        let parked = self
            .store
            .claims_for(&subject.subject, Some("runtime.action.failed"))?
            .into_iter()
            .rev()
            .find(|claim| {
                claim.store_index > reset
                    && claim.body["fields"]["action"] == RECOVERY
                    && claim.body["fields"]["desired_token"] == token
            });
        if let Some(parked) = parked {
            if let Some(observation) = observation.filter(|item| item.status == "running") {
                self.record_member(subject, observation, true)?;
            }
            self.reconcile_runtime_stop(
                &subject.subject,
                &member.runtime_id,
                member.terminal,
                observation
                    .and_then(|item| item.incarnation_id.as_deref())
                    .or_else(|| claim_incarnation(&parked)),
                member.shutdown_timeout_ms,
                observation,
            )?;
            anyhow::bail!(
                "{}",
                parked.body["fields"]["reason"].as_str().unwrap_or(RECOVERY)
            );
        }
        let Some(observation) = observation.filter(|item| item.status == "running") else {
            return Ok(false);
        };
        let Some(incarnation) = observation.incarnation_id.as_deref() else {
            return Ok(false);
        };
        // Initialization can race the recorded request. If it lands before the restart's
        // physical stop, settle that request so the ordinary action path leaves this seat alone.
        if let Some(request) = attempts
            .iter()
            .rev()
            .find(|claim| claim_incarnation(claim) == Some(incarnation))
            && (self
                .store
                .claude_channel_attached(&subject.subject, incarnation)?
                || crate::api::claude_channel_attached(&self.store, &subject.subject, incarnation))
            && !self
                .store
                .observations_for(&subject.subject, "runtime.action.requested")?
                .iter()
                .any(|claim| {
                    claim.body["fields"]["action"] == "terminate"
                        && claim_incarnation(claim) == Some(incarnation)
                })
        {
            self.store.append_claim(&ClaimInput {
                subject: subject.subject.clone(), kind: "runtime.action.succeeded".into(),
                actor: request.actor.clone(),
                fields: BTreeMap::from([
                    ("action".into(), json!("restart")),
                    ("incarnation_id".into(), json!(incarnation)),
                    ("reason".into(), json!("Claude's channel attached before the automatic restart; the existing incarnation is retained")),
                ]), evidence: vec![request.id.clone()], expected_subject: None,
                idempotency_key: Some(format!("agent-restart-completed:{}", request.id)),
            })?;
            self.signal_changed();
            return Ok(false);
        }
        let Some(harness) = self
            .store
            .current_harness(&subject.subject)?
            .filter(|harness| {
                harness.incarnation_id == incarnation
                    && harness.state == "blocked"
                    && harness.reason.as_deref() == Some("claude-channel-unattached")
            })
        else {
            return Ok(false);
        };
        // A recorded request owns this incarnation's stop/start, even if it is still in flight.
        if attempts
            .iter()
            .any(|claim| claim_incarnation(claim) == Some(incarnation))
        {
            return Ok(false);
        }
        // The attachment report may be newer than the driver's diagnostic. Recheck the live
        // current delivery fence directly before charging a retry or parking the seat.
        if crate::api::claude_channel_attached(&self.store, &subject.subject, incarnation) {
            self.arm_restart(
                &format!("channel-recovery:{}", subject.subject),
                now + 1_000,
            );
            return Ok(false);
        }
        let due = harness
            .since_unix_ms
            .saturating_add(if attempts.len() >= MAX_ATTEMPTS {
                0
            } else {
                RECHECK_BACKOFF_MS * (1_u128 << attempts.len())
            });
        if now < due {
            self.arm_restart(&format!("channel-recovery:{}", subject.subject), due);
            return Ok(false);
        }
        // Rendering and ownership must still be valid before requesting any physical action.
        if let Some(error) = blocked {
            anyhow::bail!("Claude channel recovery is blocked: {error:#}");
        }
        self.store.owned_desired_guard(subject)?;
        if attempts.len() >= MAX_ATTEMPTS {
            let reason = format!(
                "claude-channel-unattached: automatic channel recovery failed after {MAX_ATTEMPTS} restarts; the seat is parked and mail remains held. Check the Claude plugin and channel, then restart the seat."
            );
            self.store.append_claim(&ClaimInput {
                subject: subject.subject.clone(),
                kind: "runtime.action.failed".into(),
                actor: Some(RECONCILER_ACTOR.into()),
                fields: BTreeMap::from([
                    ("action".into(), json!(RECOVERY)),
                    ("desired_token".into(), json!(token)),
                    ("incarnation_id".into(), json!(incarnation)),
                    ("reason".into(), json!(reason)),
                ]),
                evidence: attempts.iter().map(|claim| claim.id.clone()).collect(),
                expected_subject: None,
                idempotency_key: Some(format!(
                    "{RECOVERY}:park:{}:{token}:{incarnation}",
                    subject.subject
                )),
            })?;
        } else {
            self.store.append_claim(&ClaimInput {
                subject: subject.subject.clone(), kind: "runtime.action.requested".into(),
                actor: Some(RECONCILER_ACTOR.into()),
                fields: BTreeMap::from([
                    ("action".into(), json!("restart")),
                    ("operation".into(), json!(RECOVERY)),
                    ("runtime_id".into(), json!(member.runtime_id)),
                    ("incarnation_id".into(), json!(incarnation)),
                    ("reason".into(), json!(format!("Automatic Claude channel recovery attempt {} of {MAX_ATTEMPTS}: the channel remained unattached during recheck; restart and continue the native session", attempts.len() + 1))),
                ]),
                evidence: vec![token.clone(), harness.claim], expected_subject: None,
                idempotency_key: Some(format!("{RECOVERY}:restart:{}:{token}:{incarnation}:{}", subject.subject, harness.since_unix_ms)),
            })?;
        }
        self.signal_changed();
        Ok(true)
    }
}
