use super::*;
use serde_json::json;

impl<R: RuntimeControl> Reconciler<R> {
    pub(super) fn reconcile_turn_obligation(
        &self,
        subject: &DesiredSubject,
        observation: &RuntimeObservation,
    ) -> Result<()> {
        let Some(incarnation) = observation.incarnation_id.as_deref() else {
            return Ok(());
        };
        let Some(revision) = self.store.selected_desired_token(&subject.subject)? else {
            return Ok(());
        };
        let recovery = match self.store.current_harness(&subject.subject)?.and_then(|harness| harness.turn_recovery) {
            Some(recovery) => Some(recovery),
            None => self.store.turn_obligation_source_at(&subject.subject, i64::MAX as u64)?
                .map(|source| json!({"state":"recovery-blocked","source_claim":source["source_claim"],"owner_action_required":source["evidence"]["owner_action_required"],"evidence":source["evidence"]})),
        };
        let receipts = recovery
            .as_ref()
            .map(|value| crate::store::turn_obligation::actionable_receipt_keys(&value["evidence"]))
            .unwrap_or_default();
        let scope = hex::encode(sha2::Sha256::digest(serde_json::to_vec(&receipts)?));
        let key = format!(
            "turn-obligation:{}:{revision}:{incarnation}:{scope}",
            subject.subject
        );
        let digest = hex::encode(sha2::Sha256::digest(key.as_bytes()));
        let episode = format!("attention/{}", &digest[..32]);
        let blocked = recovery.as_ref().is_some_and(|recovery| {
            recovery["owner_action_required"] != false
                && !matches!(
                    recovery["state"].as_str(),
                    Some("in-flight" | "pending-human")
                )
        });
        for failure in
            self.store
                .turn_obligation_failures(&subject.subject, &revision, incarnation)?
        {
            let old_keys = failure.body["fields"]["turn_receipts"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|value| value.as_str().map(str::to_owned))
                .collect::<Vec<_>>();
            if !blocked || old_keys != receipts {
                let old_episode = failure.body["fields"]["episode"]
                    .as_str()
                    .unwrap_or_default();
                if self
                    .store
                    .operational_failure(old_episode)?
                    .is_some_and(|state| state.status == "pending")
                {
                    self.store.recover_operational_failure(old_episode,
                        "this captured owner-action scope is no longer active; retained unknown execution evidence is unchanged",
                        &format!("turn-obligation-retired:{}", failure.id))?;
                    self.signal_changed();
                }
            }
        }
        if !blocked {
            return Ok(());
        }
        let recovery = recovery.unwrap();
        let pending = self
            .store
            .operational_failure(&episode)?
            .is_some_and(|failure| failure.status == "pending");
        let result = self.store.record_turn_obligation_failure(&episode, &AttentionRequest {
            reviewer: self.store.agent_person(&subject.subject)?.unwrap_or_else(|| "person/operator".into()),
            title: "An interrupted agent turn needs recovery".into(),
            reason: format!("{} has unsettled native work ({}) from {}. Inspect its saved native session and resolve the unknown outcome with its owner; automatic native continuation is unsupported.", subject.subject, recovery["state"].as_str().unwrap_or("unknown"), recovery["source_claim"].as_str().unwrap_or("unknown evidence")),
            severity: "error".into(), targets: vec![subject.subject.clone()],
            actor: RECONCILER_ACTOR.into(), idempotency_key: key,
        }, subject, &revision, incarnation);
        if let Err(error) = result {
            if matches!(
                error.code,
                "stale-observer-revision"
                    | "stale-harness-event-session"
                    | "owned-set-pending"
                    | "owned-set-conflict"
                    | "stale-set-member"
            ) {
                return Ok(());
            }
            return Err(error.into());
        }
        if !pending {
            self.signal_changed();
        }
        Ok(())
    }
}
