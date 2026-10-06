use super::*;

impl<R: RuntimeControl> Reconciler<R> {
    /// The local process belongs to this host even after its selected declaration moves away.
    pub(super) fn reconcile_placement_away(
        &self,
        subject: &DesiredSubject,
        ptys: Option<&HashMap<String, RuntimeObservation>>,
    ) -> Result<()> {
        if !subject.subject.starts_with("agent/")
            || subject.member.as_ref().is_some_and(|m| m.host == self.host)
        {
            return Ok(());
        }
        if subject.kind == "stop"
            && self
                .store
                .declaration_ended_by_stop(&subject.subject)?
                .and_then(|ended| ended.declaration.member)
                .is_none_or(|m| m.host == self.host)
        {
            // The ordinary stop path keeps retention, deadlines and incarnation fencing.
            return Ok(());
        }
        let Some(token) = self.store.selected_desired_token(&subject.subject)? else {
            return Ok(());
        };
        let mut members = BTreeMap::new();
        // Use this host's declarations, never the selected actual (which may already be
        // another host's incarnation). Keep every local runtime ID in case configuration changed.
        for claim in self
            .store
            .claims_for(&subject.subject, Some("intent.desired"))?
        {
            if let Ok(declared) = serde_json::from_value::<DesiredSubject>(claim.body)
                && let Some(member) = declared.member
                && member.host == self.host
            {
                members.insert(member.runtime_id.clone(), member);
            }
        }
        let latest = crate::placement::latest_by_origin(&self.store, &subject.subject, u64::MAX)?
            .remove(&self.host);
        let mut observations = BTreeMap::new();
        for member in members.values() {
            let observation = if member.terminal {
                smallclaims::touched::note_read(|| format!("pty:{}", member.runtime_id));
                let Some(ptys) = ptys else { return Ok(()) };
                ptys.get(&member.runtime_id).cloned()
            } else {
                smallclaims::touched::note_read(|| format!("exec:{}", member.runtime_id));
                self.runtime.observe_exec(&member.runtime_id)?
            };
            observations.insert(member.runtime_id.clone(), observation);
        }
        if members.is_empty() {
            return Ok(());
        }
        // A later destination observation must not cause the source to re-emit its stop.
        let all_stopped = observations.values().all(|o| {
            o.as_ref()
                .is_none_or(|o| o.status != "running" && o.status != "unknown")
        });
        if all_stopped
            && let Some(claim) = latest.as_ref()
            && crate::placement::field(claim, "status") == Some("stopped")
        {
            let tokens = crate::placement::fence(&self.store, &subject.subject, &token)?
                .and_then(|f| f.sources.get(&self.host).cloned())
                .unwrap_or_else(|| BTreeSet::from([token.clone()]));
            if crate::placement::acknowledges(&self.store, claim, &tokens)? {
                return Ok(());
            }
        }
        let mut stopped = true;
        for member in members.values() {
            let observation = observations
                .get(&member.runtime_id)
                .and_then(Option::as_ref);
            let incarnation = observation
                .and_then(|o| o.incarnation_id.as_deref())
                .or_else(|| {
                    latest
                        .as_ref()
                        .filter(|c| {
                            crate::placement::field(c, "runtime_id")
                                == Some(member.runtime_id.as_str())
                        })
                        .and_then(|c| crate::placement::field(c, "incarnation_id"))
                });
            if let Some(observation) =
                observation.filter(|o| matches!(o.status.as_str(), "running" | "unknown"))
            {
                // A new live or unreadable incarnation revokes an older acknowledgement.
                // Publish it before sending a signal; a signal is not evidence of exit.
                let fields = member_fields(member, &observation.status, incarnation, true);
                if latest.as_ref().is_none_or(|c| {
                    c.body.get("fields") != Some(&serde_json::to_value(&fields).unwrap())
                }) {
                    self.record_once(&subject.subject, "runtime.observed", fields)?;
                }
                if subject.kind != "stop"
                    && self.defer_declared_restart(subject, observation, now_ms())?
                {
                    stopped = false;
                    continue;
                }
                stopped &= self.reconcile_runtime_stop(
                    &subject.subject,
                    &member.runtime_id,
                    member.terminal,
                    incarnation,
                    member.shutdown_timeout_ms,
                    Some(observation),
                )?;
            } else {
                // Do not let a terminal record for an older runtime ID hide a still-live
                // local incarnation. One acknowledgement is written only after all stop.
                self.runtime
                    .end_leftovers(&member.runtime_id, member.terminal);
            }
        }
        if stopped {
            let member = latest
                .as_ref()
                .and_then(|c| crate::placement::field(c, "runtime_id"))
                .and_then(|id| members.get(id))
                .unwrap_or_else(|| members.values().next().unwrap());
            let incarnation = observations
                .get(&member.runtime_id)
                .and_then(Option::as_ref)
                .and_then(|o| o.incarnation_id.as_deref())
                .or_else(|| {
                    latest
                        .as_ref()
                        .and_then(|c| crate::placement::field(c, "incarnation_id"))
                });
            let mut fields = member_fields(member, "stopped", incarnation, false);
            fields.insert("reason".into(), Value::String("placed-elsewhere".into()));
            let mut evidence = vec![token];
            if let Some(local) =
                crate::placement::latest_by_origin(&self.store, &subject.subject, u64::MAX)?
                    .remove(&self.host)
            {
                evidence.push(local.id);
            }
            self.record_once_with_evidence(&subject.subject, "runtime.observed", fields, evidence)?;
        }
        Ok(())
    }

    pub(super) fn placement_start_evidence(&self, subject: &str) -> Result<Option<Vec<String>>> {
        let Some(token) = self.store.selected_desired_token(subject)? else {
            return Ok(Some(Vec::new()));
        };
        let Some(fence) = crate::placement::fence(&self.store, subject, &token)? else {
            return Ok(Some(Vec::new()));
        };
        if fence.sources.is_empty() {
            return Ok(Some(Vec::new()));
        }
        let latest = crate::placement::latest_by_origin(&self.store, subject, u64::MAX)?;
        let overrides =
            crate::placement::source_offline_overrides(&self.store, subject, &fence, u64::MAX)?;
        let mut evidence = Vec::new();
        for (source, tokens) in &fence.sources {
            if let Some(claim) = latest.get(source)
                && crate::placement::field(claim, "status") == Some("stopped")
                && crate::placement::acknowledges(&self.store, claim, tokens)?
            {
                evidence.push(claim.id.clone());
            } else if let Some(override_claim) = overrides.get(source) {
                evidence.push(override_claim.id.clone());
            } else {
                return Ok(None);
            }
        }
        evidence.sort();
        evidence.dedup();
        Ok(Some(evidence))
    }
}
