//! A dependency-admitted mission step with no desired seat has one readiness-scoped fault.
use super::*;

pub(crate) const MISSING_AGENT_CONDITION: &str = "mission-no-eligible-agent";

pub(super) fn episode(run: &MissionRunView, step: &StepRunView) -> String {
    format!(
        "{MISSING_AGENT_CONDITION}:{}:{}:{}",
        run.generation, step.step, step.readiness_epoch
    )
}

fn blocked(step: &StepRunView) -> bool {
    step.status == "blocked"
        && step
            .blocked_reason
            .as_deref()
            .is_some_and(|r| r.starts_with("no eligible agent is present"))
}

fn missing_binding_tx(connection: &Connection, step: &StepRunView) -> Result<bool> {
    let mut deliberate_stop = false;
    for name in step.assigned_to.iter().chain(step.available_to.iter()) {
        smallclaims::touched::note_read(|| name.clone());
        if let Some(row) = current_desired_row(connection, name)? {
            if row.kind == "agent" {
                return Ok(false);
            }
            deliberate_stop |=
                row.kind == "stop" && row.owner_run.is_none() && row.owner_generation.is_none();
        }
    }
    Ok(!deliberate_stop)
}

impl Store {
    /// Stop ownership is enough to preserve an existing cross-run ready queue.
    pub(crate) fn selected_stop_owner_run(&self, subject: &str) -> Result<Option<String>> {
        smallclaims::touched::note_read(|| subject.to_owned());
        Ok(self
            .readers
            .get()
            .query_row(
                "SELECT owner_run FROM desired WHERE subject=?1 AND kind='stop'",
                [subject],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten())
    }

    pub(crate) fn missing_agent_fault_is_current(
        &self,
        run: &MissionRunView,
        step: &StepRunView,
    ) -> Result<bool> {
        if run.phase != "normal"
            || !matches!(run.status.as_str(), "running" | "standing" | "blocked")
            || step.generation != run.generation
            || !blocked(step)
        {
            return Ok(false);
        }
        missing_binding_tx(&self.readers.get(), step)
    }

    /// Recheck the generation, admitted step and bindings under the writer before reporting.
    /// The indexed source lookup dedupes without replaying operational episode history.
    pub(crate) fn record_missing_agent_failure(
        &self,
        expected: &MissionRunView,
        expected_step: &StepRunView,
    ) -> Result<bool> {
        // Quiet root stops and already-reported episodes must not join the writer queue.
        // This preview grants no authority: every needed write is rechecked below.
        if !self.missing_agent_fault_is_current(expected, expected_step)? {
            return Ok(false);
        }
        let expected_episode = episode(expected, expected_step);
        if self.readers.get().query_row(
            "SELECT 1 FROM claims WHERE subject=?1 AND kind='operational.failure' AND json_extract(body, '$.fields.episode')=?2 LIMIT 1",
            params![expected.subject, expected_episode], |_| Ok(())
        ).optional()?.is_some() { return Ok(false); }
        let mut connection = self.connection.write();
        let transaction = connection.transaction()?;
        let run = mission_run_view_for_reconcile_tx(&transaction, &expected.id)?;
        if run.generation != expected.generation
            || run.revision != expected.revision
            || run.phase != "normal"
            || !matches!(run.status.as_str(), "running" | "standing" | "blocked")
        {
            return Ok(false);
        }
        let Some(step) = step_run_row_tx(&transaction, &expected_step.subject)? else {
            return Ok(false);
        };
        if step.generation != run.generation
            || step.attempt != expected_step.attempt
            || step.readiness_epoch != expected_step.readiness_epoch
            || step.claimant != expected_step.claimant
            || step.claim_incarnation != expected_step.claim_incarnation
            || !blocked(&step)
            || !missing_binding_tx(&transaction, &step)?
        {
            return Ok(false);
        }
        let episode = episode(&run, &step);
        if transaction.query_row(
            "SELECT 1 FROM claims WHERE subject=?1 AND kind='operational.failure' AND json_extract(body, '$.fields.episode')=?2 LIMIT 1",
            params![run.subject, episode], |_| Ok(())
        ).optional()?.is_some() { return Ok(false); }
        append_claim_tx(
            &transaction,
            &self.origin,
            &run.subject,
            "operational.failure",
            Some("agent/st3/reconciler"),
            &json!({"fields": {
                "episode": episode, "condition": MISSING_AGENT_CONDITION,
                "reviewer": run.requester, "title": "Mission work has no eligible agent", "severity": "error",
                "targets": [run.subject, run.generation, step.subject],
                "reason": format!("Run `{}` generation `{}` step `{}` cannot advance: {}. Declare an assigned or available agent, or revise the mission to use an existing seat. Inspect with `st missions show {}`. This fault closes when eligibility returns, the generation changes, or the run ends.", run.subject, run.generation, step.subject, step.blocked_reason.as_deref().unwrap_or_default(), run.subject)
            }}),
            &[],
            None,
        )?;
        transaction.commit()?;
        Ok(true)
    }
}
