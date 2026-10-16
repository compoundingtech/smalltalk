use super::*;

impl Store {
    /// Repository candidates on one host, excluding members with neither a checkout nor any
    /// published workspace evidence. Uses the host index rather than walking the fleet graph.
    pub(crate) fn agent_repository_subjects(&self, host: &str) -> Result<Vec<DesiredSubject>> {
        let connection = self.readers.get();
        let mut statement = connection.prepare_cached(
            "SELECT subject, kind, body, member, owner_run, owner_generation, owner_step FROM desired
             WHERE kind='agent' AND json_extract(member, '$.host')=?1
               AND (EXISTS (SELECT 1 FROM json_each(body, '$.children') child WHERE json_extract(child.value, '$.name')='checkout')
                    OR EXISTS (SELECT 1 FROM claims WHERE claims.subject=desired.subject AND claims.kind='workspace.observed'))
             ORDER BY subject",
        )?;
        let rows = statement.query_map([host], desired_from_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Find a mission-run PR resource for this discovered PR when that run already has a human
    /// review gate for its own PR. Intake uses this graph provenance to avoid a second reviewer.
    /// Why a pull request review that has not started must not start: since the request, its
    /// pull request closed or merged, went back to draft, or moved to a newer head. `None` while
    /// the requested head is still the open pull request's current head.
    pub fn stale_pull_request_request(
        &self,
        resource: &str,
        discovery: &str,
    ) -> Result<Option<String>> {
        stale_pull_request_request_tx(&self.readers.get(), resource, discovery)
    }

    /// Only subscription deliveries follow the current PR head. Authored missions keep their
    /// pinned inputs, including missions that opened or are changing a pull request themselves.
    pub fn stale_subscription_pull_request_run(
        &self,
        run: &MissionRunView,
    ) -> Result<Option<String>> {
        if !run.inputs.values().any(|input| {
            input.subject.as_deref().is_some_and(|subject| subject.contains("/pull-request/"))
        }) {
            return Ok(None);
        }
        let from_subscription = run
            .parent_step_run
            .as_deref()
            .is_some_and(|parent| parent.starts_with("step-run/subscription/"));
        if !from_subscription {
            smallclaims::touched::note_read(|| format!("subscription-run:{}", run.subject));
            let started = subscription_started_run_tx(&self.readers.get(), &run.subject)?;
            if !started {
                return Ok(None);
            }
        }
        for input in run
            .inputs
            .values()
            .filter(|input| input.kind == MissionInputKind::Resource)
        {
            if let (Some(resource), Some(discovery)) = (&input.subject, &input.claim_id)
                && let Some(reason) = self.stale_pull_request_request(resource, discovery)?
            {
                return Ok(Some(reason));
            }
        }
        Ok(None)
    }
}
