use super::*;
use crate::rollout::{self, Operation};

impl<R: RuntimeControl> Reconciler<R> {
    pub(super) fn rollout_render_guard(&self, subject: &DesiredSubject) -> Result<()> {
        self.store.owned_desired_guard(subject)?;
        let Some(operation) = self.store.rollout(&subject.subject)? else {
            return Ok(());
        };
        if operation.phase == "starting" {
            anyhow::ensure!(
                self.runtime.rollout_exited(
                    &operation.old_member.runtime_id,
                    operation.old_member.terminal,
                    &operation.old_incarnation
                )?,
                "rollout render waits for positive exit of the original incarnation"
            );
        }
        if operation.phase == "verifying" {
            let observation = self
                .runtime
                .snapshot_ptys()?
                .into_iter()
                .find(|o| o.runtime_id == operation.old_member.runtime_id);
            anyhow::ensure!(
                observation.is_some_and(|o| o.status == "running"
                    && o.incarnation_id == operation.replacement_incarnation),
                "rollout render waits for the exact replacement incarnation"
            );
        }
        Ok(())
    }

    /// Own the entire cutover before ordinary restart, suspension or restart policy can act.
    pub(super) fn reconcile_rollout(
        &self,
        subject: &DesiredSubject,
        observed: Option<&RuntimeObservation>,
        blocked: Option<&anyhow::Error>,
    ) -> Result<bool> {
        let Some(selected) = self.store.rollout_selection(&subject.subject)? else {
            return Ok(false);
        };
        self.store.owned_desired_guard(subject)?;
        let mut operation = self.store.rollout(&subject.subject)?;
        // PTY registries can remove an exited record. Only the runtime adapter may certify
        // that the exact original process exited; absence alone remains unknown.
        let normalized;
        let observed = if let Some(operation) = &operation
            && (observed.is_none()
                || observed.is_some_and(|o| {
                    o.incarnation_id.is_none()
                        && matches!(o.status.as_str(), "exited" | "stopped" | "vanished")
                }))
            && self.runtime.rollout_exited(
                &operation.old_member.runtime_id,
                operation.old_member.terminal,
                &operation.old_incarnation,
            )? {
            normalized = RuntimeObservation {
                runtime_id: operation.old_member.runtime_id.clone(),
                terminal: operation.old_member.terminal,
                status: "exited".into(),
                exit_code: None,
                incarnation_id: Some(operation.old_incarnation.clone()),
            };
            Some(&normalized)
        } else {
            observed
        };
        if operation.as_ref().is_none_or(|o| o.phase == "superseded") {
            let positive_exit = observed
                .is_some_and(|o| matches!(o.status.as_str(), "exited" | "stopped" | "vanished"));
            let carry = operation
                .as_ref()
                .is_some_and(|o| o.native_session_id.is_some());
            let Some(observation) =
                observed.filter(|o| o.status == "running" || (positive_exit && carry))
            else {
                return Ok(operation
                    .as_ref()
                    .is_some_and(|o| o.start_attempted || o.native_session_id.is_some())
                    || rollout::hold_render(&self.store, subject)?);
            };
            let Some(incarnation) = observation.incarnation_id.as_deref() else {
                return Ok(true);
            };
            let Some((_, old)) =
                rollout::launched_member(&self.store, &subject.subject, incarnation)?
            else {
                // Adopted native seats without start lineage cannot be cut over safely.
                return Ok(subject.kind == "stop" || rollout::hold_render(&self.store, subject)?);
            };
            if !carry
                && selected
                    .desired
                    .member
                    .as_ref()
                    .is_some_and(|new| new.launch_changes(&old).is_empty())
            {
                return Ok(false);
            }
            if old.host != self.host {
                return Ok(false);
            }
            self.record_once(
                &subject.subject,
                "runtime.observed",
                member_fields(
                    &old,
                    if positive_exit { "stopped" } else { "running" },
                    observation.incarnation_id.as_deref(),
                    true,
                ),
            )?;
            let actor = selected
                .actor
                .as_deref()
                .context("rollout publication needs its actor")?;
            let key = format!(
                "seat-rollout:{}:{}:{}",
                selected.set,
                selected.target,
                smallclaims::hash::canonical_hash(&(incarnation, &selected.policy))?
            );
            self.store.request_rollout(
                &subject.subject,
                &selected.desired_token,
                &old,
                incarnation,
                actor,
                &selected.policy,
                &key,
            )?;
            self.signal_changed();
            operation = self.store.rollout(&subject.subject)?;
        }
        let Some(mut operation) = operation else {
            return Ok(true);
        };
        if matches!(operation.phase.as_str(), "running" | "retired") {
            return Ok(false);
        }
        if operation.old_member.host != self.host {
            return Ok(true);
        }
        let agent = &subject.subject;
        let old = &operation.old_member;
        let phase = |operation: &Operation,
                     state: &str,
                     reason: Option<&str>,
                     blockers: &[String]|
         -> Result<()> {
            rollout::phase(&self.store, agent, operation, state, reason, blockers)?;
            self.signal_changed();
            Ok(())
        };
        let current = || -> Result<bool> {
            self.store.owned_desired_guard(subject)?;
            Ok(self
                .store
                .rollout(agent)?
                .is_some_and(|o| o.id == operation.id && o.phase != "superseded"))
        };
        if !current()? {
            return Ok(true);
        }
        if matches!(
            operation.phase.as_str(),
            "held" | "failed" | "blocked" | "superseded"
        ) {
            if operation.phase == "failed" {
                let mut failed = operation.clone();
                if failed.replacement_incarnation.is_none()
                    && let Some(observation) = observed.filter(|o| o.status == "running")
                    && let Some(incarnation) = observation
                        .incarnation_id
                        .as_deref()
                        .filter(|inc| *inc != operation.old_incarnation)
                    && self
                        .runtime
                        .rollout_operation(&observation.runtime_id, incarnation)?
                        .as_deref()
                        == Some(operation.id.as_str())
                {
                    failed.replacement_incarnation = Some(incarnation.into());
                    phase(
                        &failed,
                        "failed-replacement",
                        failed.reason.as_deref(),
                        &failed.blocking,
                    )?;
                }
                if let Some(replacement) = failed.replacement_incarnation.as_deref() {
                    if let Some(observation) = observed.filter(|o| {
                        o.status == "running" && o.incarnation_id.as_deref() == Some(replacement)
                    }) {
                        self.reconcile_runtime_stop(
                            agent,
                            &observation.runtime_id,
                            observation.terminal,
                            Some(replacement),
                            old.shutdown_timeout_ms,
                            Some(observation),
                        )?;
                    } else if self.runtime.rollout_exited(
                        &old.runtime_id,
                        old.terminal,
                        replacement,
                    )? {
                        let member = subject.member.as_ref().unwrap_or(old);
                        self.record_once(
                            agent,
                            "runtime.observed",
                            member_fields(member, "stopped", Some(replacement), false),
                        )?;
                    }
                } else if self.runtime.rollout_exited(
                    &old.runtime_id,
                    old.terminal,
                    &operation.old_incarnation,
                )? {
                    self.record_once(
                        agent,
                        "runtime.observed",
                        member_fields(old, "stopped", Some(&operation.old_incarnation), false),
                    )?;
                }
            }
            return Ok(true);
        }
        self.arm_restart(
            &format!("seat-rollout:{agent}"),
            now_ms().saturating_add(1_000),
        );
        match operation.phase.as_str() {
            "draining" => {
                let Some(observation) = observed.filter(|o| o.status == "running") else {
                    phase(
                        &operation,
                        "blocked",
                        Some("the original runtime is no longer observed running"),
                        &["runtime-unknown".into()],
                    )?;
                    return Ok(true);
                };
                if observation.incarnation_id.as_deref() != Some(operation.old_incarnation.as_str())
                {
                    phase(
                        &operation,
                        "blocked",
                        Some("the live incarnation differs from the drain fence"),
                        &["stale-incarnation".into()],
                    )?;
                    return Ok(true);
                }
                self.record_once(
                    agent,
                    "runtime.observed",
                    member_fields(old, "running", observation.incarnation_id.as_deref(), true),
                )?;
                let mut blockers = rollout::blockers(&self.store, agent, &operation)?;
                let expired = now_ms() >= operation.deadline_unix_ms;
                if expired && !operation.policy.force_after_deadline {
                    phase(
                        &operation,
                        "held",
                        Some("drain deadline reached; the original seat keeps running"),
                        &blockers,
                    )?;
                    return Ok(true);
                }
                if blocked.is_some() {
                    blockers.push("render-invalid".into());
                }
                if subject
                    .member
                    .as_ref()
                    .is_some_and(|m| crate::native_resume::rollout_support(m).is_err())
                {
                    blockers.push("native-launch-unsupported".into());
                }
                let identity_blocked = blockers.iter().any(|b| {
                    matches!(
                        b.as_str(),
                        "native-session-unbound"
                            | "drain-unacknowledged"
                            | "render-invalid"
                            | "native-launch-unsupported"
                            | "harness-unobserved"
                            | "quiescence-unreported"
                            | "starting"
                            | "harness-indeterminate"
                    )
                });
                let force = expired && operation.policy.force_after_deadline;
                if !blockers.is_empty() && (!force || identity_blocked) {
                    return Ok(true);
                }
                let Some((harness, session, path)) =
                    rollout::binding(&self.store, agent, &operation.old_incarnation)?
                else {
                    return Ok(true);
                };
                if Some(harness.as_str()) != old.driver.as_deref() {
                    phase(
                        &operation,
                        "blocked",
                        Some("native binding has a different harness"),
                        &["native-harness-mismatch".into()],
                    )?;
                    return Ok(true);
                }
                if !current()? {
                    return Ok(true);
                }
                let native_account =
                    rollout::bound_account(&self.store, agent, &operation.old_incarnation)?;
                if operation
                    .native_session_id
                    .as_deref()
                    .is_some_and(|original| {
                        original != session || operation.native_account != native_account
                    })
                {
                    phase(
                        &operation,
                        "blocked",
                        Some("incumbent does not bind the original conversation"),
                        &["native-session-mismatch".into()],
                    )?;
                    return Ok(true);
                }
                operation.native_session_id = Some(session);
                operation.native_account = native_account;
                operation.native_path = path.or(operation.native_path.clone());
                operation.forced = force;
                phase(&operation, "stopping", None, &blockers)?;
            }
            "stopping" => {
                let Some(observation) = observed else {
                    return Ok(true);
                };
                if observation.incarnation_id.as_deref() != Some(operation.old_incarnation.as_str())
                {
                    phase(
                        &operation,
                        "blocked",
                        Some("the runtime identity changed while stopping"),
                        &["stale-incarnation".into()],
                    )?;
                    return Ok(true);
                }
                if observation.status == "running" {
                    // Quiescence can change after the snapshot; repeat it before every signal.
                    if !operation.forced {
                        let blockers = rollout::blockers(&self.store, agent, &operation)?;
                        if !blockers.is_empty() {
                            if now_ms() >= operation.deadline_unix_ms {
                                phase(
                                    &operation,
                                    "held",
                                    Some("drain deadline reached before stop"),
                                    &blockers,
                                )?;
                            }
                            return Ok(true);
                        }
                    }
                    // The durable phase retains the original fence on every later pass.
                    if !current()? {
                        return Ok(true);
                    }
                    self.reconcile_runtime_stop(
                        agent,
                        &old.runtime_id,
                        old.terminal,
                        Some(&operation.old_incarnation),
                        old.shutdown_timeout_ms,
                        Some(observation),
                    )?;
                } else if matches!(
                    observation.status.as_str(),
                    "exited" | "vanished" | "stopped"
                ) {
                    self.runtime.end_leftovers(&old.runtime_id, old.terminal);
                    self.record_once(
                        agent,
                        "runtime.observed",
                        member_fields(old, "stopped", observation.incarnation_id.as_deref(), false),
                    )?;
                    if !current()? {
                        return Ok(true);
                    }
                    phase(
                        &operation,
                        if subject.kind == "stop" {
                            "retired"
                        } else {
                            "starting"
                        },
                        None,
                        &[],
                    )?;
                }
            }
            "starting" => {
                let start = self
                    .store
                    .observations_for(agent, "runtime.action.succeeded")?
                    .into_iter()
                    .rev()
                    .find(|c| {
                        c.body
                            .pointer("/fields/rollout_operation")
                            .and_then(Value::as_str)
                            == Some(operation.id.as_str())
                            && c.body.pointer("/fields/action").and_then(Value::as_str)
                                == Some("start")
                    });
                if let Some(start) = start {
                    operation.replacement_incarnation = start
                        .body
                        .pointer("/fields/incarnation_id")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    if operation.replacement_incarnation.is_some() && current()? {
                        operation.desired_token = start
                            .body
                            .pointer("/fields/desired_token")
                            .and_then(Value::as_str)
                            .context("start receipt has no desired token")?
                            .into();
                        phase(&operation, "verifying", None, &[])?;
                        return Ok(true);
                    }
                }
                if operation.start_attempted {
                    // A crash after spawn can be recovered only through the driver's operation marker.
                    if let Some(observation) = observed.filter(|o| o.status == "running")
                        && let Some(incarnation) = observation.incarnation_id.as_deref()
                        && incarnation != operation.old_incarnation
                    {
                        let report = self
                            .store
                            .latest_observation(agent, "harness.observed")?
                            .filter(|report| {
                                report
                                    .body
                                    .pointer("/fields/incarnation_id")
                                    .and_then(Value::as_str)
                                    == Some(incarnation)
                                    && report
                                        .body
                                        .pointer("/fields/rollout_operation")
                                        .and_then(Value::as_str)
                                        == Some(operation.id.as_str())
                            });
                        let physical = self
                            .runtime
                            .rollout_operation(&observation.runtime_id, incarnation)?
                            .as_deref()
                            == Some(operation.id.as_str());
                        if !physical && report.is_none() {
                            return Ok(true);
                        }
                        if !current()? {
                            return Ok(true);
                        }
                        self.store.append_claim(&ClaimInput {
                            subject: agent.into(),
                            kind: "runtime.action.succeeded".into(),
                            actor: operation.requested_by.clone(),
                            fields: BTreeMap::from([
                                ("action".into(), serde_json::json!("start")),
                                (
                                    "desired_token".into(),
                                    serde_json::json!(operation.desired_token),
                                ),
                                ("incarnation_id".into(), serde_json::json!(incarnation)),
                                ("rollout_operation".into(), serde_json::json!(operation.id)),
                            ]),
                            evidence: std::iter::once(operation.id.clone())
                                .chain(report.map(|r| r.id))
                                .collect(),
                            expected_subject: None,
                            idempotency_key: Some(format!(
                                "seat-rollout-start-recovered:{}",
                                operation.id
                            )),
                        })?;
                        operation.replacement_incarnation = Some(incarnation.into());
                        phase(&operation, "verifying", None, &[])?;
                    } else if now_ms()
                        >= operation
                            .phase_at_unix_ms
                            .saturating_add(crate::suspension::VERIFY_TIMEOUT_MS)
                    {
                        phase(
                            &operation,
                            "blocked",
                            Some(
                                "start was attempted but its replacement cannot be identified; retry after observing it",
                            ),
                            &["replacement-unknown".into()],
                        )?;
                    }
                    return Ok(true);
                }
                if observed.is_none_or(|o| {
                    !matches!(o.status.as_str(), "exited" | "vanished" | "stopped")
                        || o.incarnation_id.as_deref() != Some(operation.old_incarnation.as_str())
                }) {
                    return Ok(true);
                }
                if let Some(error) = blocked {
                    phase(
                        &operation,
                        "failed",
                        Some(&format!("render blocked: {error:#}")),
                        &["render-invalid".into()],
                    )?;
                    return Ok(true);
                }
                if !current()? {
                    return Ok(true);
                }
                let mut member = subject
                    .member
                    .clone()
                    .context("rollout replacement has no launch")?;
                member.environment.insert(
                    crate::suspension::RESUME_ENV.into(),
                    operation
                        .native_session_id
                        .clone()
                        .context("rollout snapshot has no session")?,
                );
                member
                    .environment
                    .insert(rollout::OPERATION_ENV.into(), operation.id.clone());
                member.environment.insert(
                    rollout::PREDECESSOR_ENV.into(),
                    operation.old_incarnation.clone(),
                );
                if let Some(path) = &operation.native_path {
                    member
                        .environment
                        .insert(rollout::RESUME_PATH_ENV.into(), path.clone());
                }
                operation.desired_token = self
                    .store
                    .selected_desired_token(agent)?
                    .context("replacement has no desired token")?;
                phase(&operation, "start-attempted", None, &[])?;
                if !current()? {
                    return Ok(true);
                }
                if let Err(error) = self.perform_start(
                    subject,
                    &member,
                    "owned-set rollout resumed its native conversation",
                ) {
                    phase(
                        &operation,
                        "failed",
                        Some(&format!("start failed: {error:#}")),
                        &[],
                    )?;
                }
            }
            "verifying" => {
                let Some(replacement) = operation.replacement_incarnation.as_deref() else {
                    return Ok(true);
                };
                let Some(observation) = observed else {
                    return Ok(true);
                };
                if observation.incarnation_id.as_deref() != Some(replacement) {
                    phase(
                        &operation,
                        "blocked",
                        Some("replacement identity changed during verification"),
                        &["stale-incarnation".into()],
                    )?;
                    return Ok(true);
                }
                let binding = rollout::binding(&self.store, agent, replacement)?;
                let refused = self
                    .store
                    .latest_observation(agent, "harness.diagnostic")?
                    .is_some_and(|c| {
                        c.body
                            .pointer("/fields/incarnation_id")
                            .and_then(Value::as_str)
                            == Some(replacement)
                            && c.body.pointer("/fields/code").and_then(Value::as_str)
                                == Some(crate::suspension::RESUME_UNAVAILABLE_CODE)
                    });
                let account_mismatch = binding.is_some()
                    && rollout::bound_account(&self.store, agent, replacement)?
                        != operation.native_account;
                let mismatch = binding.as_ref().is_some_and(|(harness, session, _)| {
                    Some(harness.as_str()) != old.driver.as_deref()
                        || Some(session.as_str()) != operation.native_session_id.as_deref()
                });
                if refused
                    || mismatch
                    || account_mismatch
                    || observation.status != "running"
                    || now_ms()
                        >= operation
                            .phase_at_unix_ms
                            .saturating_add(crate::suspension::VERIFY_TIMEOUT_MS)
                {
                    phase(
                        &operation,
                        "failed",
                        Some("replacement could not verify the original native conversation"),
                        &["native-verification-failed".into()],
                    )?;
                    if observation.status == "running" && current()? {
                        self.reconcile_runtime_stop(
                            agent,
                            &observation.runtime_id,
                            observation.terminal,
                            Some(replacement),
                            old.shutdown_timeout_ms,
                            Some(observation),
                        )?;
                    }
                } else if binding.is_some() && current()? {
                    let start = self
                        .store
                        .observations_for(agent, "runtime.action.succeeded")?
                        .iter()
                        .any(|c| {
                            c.body.pointer("/fields/action").and_then(Value::as_str)
                                == Some("start")
                                && c.body
                                    .pointer("/fields/incarnation_id")
                                    .and_then(Value::as_str)
                                    == Some(replacement)
                                && c.body
                                    .pointer("/fields/desired_token")
                                    .and_then(Value::as_str)
                                    == Some(operation.desired_token.as_str())
                        });
                    if start {
                        self.record_member(subject, observation, true)?;
                        phase(&operation, "running", None, &[])?;
                    }
                }
            }
            _ => {}
        }
        Ok(true)
    }
}
