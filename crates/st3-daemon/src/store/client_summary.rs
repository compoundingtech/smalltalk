//! Lean native mission inputs for the tiny summary. Presentation uses the shared
//! UI semantics; these inputs omit card goals, history, reasons and provenance.
use super::*;

type SummaryRunHeader = (String, String, String, String, String, String);

const FUTURE_ASK_SAME_WIDTH: &str = "SELECT accepted_at_unix_ms FROM claims INDEXED BY claims_accepted_order_index WHERE kind='work.person-asked' AND length(accepted_at_unix_ms)=length(?1) AND accepted_at_unix_ms>?1 ORDER BY accepted_at_unix_ms,store_index LIMIT 1";
const FUTURE_ASK_LONGER: &str = "SELECT accepted_at_unix_ms FROM claims INDEXED BY claims_accepted_order_index WHERE kind='work.person-asked' AND length(accepted_at_unix_ms)>length(?1) ORDER BY length(accepted_at_unix_ms),accepted_at_unix_ms,store_index LIMIT 1";

impl Store {
    pub(crate) fn client_summary_missions(
        &self,
        at: u128,
    ) -> Result<(Vec<st3_client::Mission>, Option<u128>)> {
        let c = self.readers.get();
        // Published definitions with no run always read NotStarted and cannot be active.
        let candidates: Vec<String> = c.prepare_cached(
            "WITH states AS (
                SELECT mission_id,MAX(status='running') running,MAX(status='standing') standing
                FROM mission_runs GROUP BY mission_id
             ), latest AS (
                SELECT mission_id,status,updated_at_unix_ms,
                       ROW_NUMBER() OVER(PARTITION BY mission_id ORDER BY created_at_unix_ms DESC,id DESC) rank
                FROM mission_runs
             )
             SELECT states.mission_id FROM states
             LEFT JOIN mission_definitions def ON def.mission_id=states.mission_id
             JOIN latest ON latest.mission_id=states.mission_id AND latest.rank=1
             WHERE states.mission_id NOT LIKE '__st3/%'
               AND (CASE WHEN states.running THEN 'running' WHEN states.standing THEN 'standing'
                         WHEN def.state='retired' THEN 'retired' ELSE latest.status END
                         NOT IN ('completed','failed','cancelled','retired')
                    OR (latest.status IN ('failed','cancelled') AND COALESCE(def.state,'')<>'retired'
                        AND NOT states.running AND NOT states.standing
                        AND CAST(latest.updated_at_unix_ms AS INTEGER)>=?1))
             ORDER BY states.mission_id")?
            .query_map([at.saturating_sub(RECENTLY_ENDED_MS) as i64], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        // A conservative deadline includes all worker leases: a selected person's
        // effective state can depend on an origin outside the selected preview.
        let lease: Option<String> = c.query_row(
            "SELECT CAST(MIN(CAST(lease_expires_at_unix_ms AS INTEGER)) AS TEXT) FROM step_runs WHERE CAST(lease_expires_at_unix_ms AS INTEGER)>CAST(?1 AS INTEGER)",
            [at.to_string()], |r| r.get(0))?;
        let mut deadline = lease.map(|s| s.parse::<u128>()).transpose()?;
        // The tuple comparison cannot seek this expression index in SQLite.
        // Separate equal-width and longer-width ranges skip historical claims.
        for sql in [FUTURE_ASK_SAME_WIDTH, FUTURE_ASK_LONGER] {
            let next: Option<String> = c
                .query_row(sql, [at.to_string()], |r| r.get(0))
                .optional()?;
            if let Some(next) = next {
                let next = next.parse::<u128>()?;
                deadline = Some(deadline.map_or(next, |held| held.min(next)));
            }
        }
        let mut result = Vec::new();
        for mission in candidates {
            let retired: bool = c
                .query_row(
                    "SELECT state='retired' FROM mission_definitions WHERE mission_id=?1",
                    [&mission],
                    |r| r.get(0),
                )
                .optional()?
                .unwrap_or(false);
            let (running,standing):(bool,bool)=c.query_row("SELECT COALESCE(MAX(status='running'),0),COALESCE(MAX(status='standing'),0) FROM mission_runs WHERE mission_id=?1",[&mission],|r|Ok((r.get(0)?,r.get(1)?)))?;
            let rows:Vec<SummaryRunHeader>=c.prepare_cached("SELECT id,current_generation_id,requester,status,phase,updated_at_unix_ms FROM mission_runs WHERE mission_id=?1 ORDER BY created_at_unix_ms DESC,id DESC LIMIT 3")?.query_map([&mission],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?)))?.collect::<rusqlite::Result<_>>()?;
            if let Some(latest) = rows.first()
                && matches!(latest.3.as_str(), "failed" | "cancelled")
            {
                let expiry = latest
                    .5
                    .parse::<u128>()?
                    .saturating_add(RECENTLY_ENDED_MS)
                    .saturating_add(1);
                if expiry > at {
                    deadline = Some(deadline.map_or(expiry, |held| held.min(expiry)));
                }
            }
            let mut state = if running {
                "running"
            } else if standing {
                "standing"
            } else if retired {
                "retired"
            } else {
                rows.first().map(|r| r.3.as_str()).unwrap_or("ready")
            }
            .to_owned();
            let mut details = Vec::new();
            for (id, generation, requester, status, phase, _) in rows.into_iter().rev() {
                let subject = format!("mission-run/{id}");
                let (_, _, steps) = self.mission_step_preview_at(&subject, at)?;
                if self
                    .reconcile_fault(&subject, crate::reconcile::FIRST_READINESS_FAULT_SCOPE)?
                    .is_some()
                {
                    state = "blocked".into();
                }
                let steps=steps.iter().map(|step|json!({"id":step.subject,"path":step.step,"state":crate::api::client_work_state(&step.status),"attempt":step.attempt,"assignee":step.assigned_to,"claimant":step.claimant,"agentless":step.agentless,"since":crate::api::client_timestamp(step.updated_at_unix_ms),"goals":[],"constraints":[],"blockers":[]})).collect::<Vec<_>>();
                details.push(json!({"id":subject,"generation_id":format!("run-generation/{generation}"),"requester":requester,"status":status,"phase":phase,"progress":{"done":0,"total":0},"current_steps":[],"must_act":"nobody","state_since":crate::api::client_timestamp(0),"steps":steps}));
            }
            result.push(serde_json::from_value(json!({"id":format!("mission/{mission}"),"kind":"mission","revision":"summary-input","updated_at":crate::api::client_timestamp(0),"title":mission,"state":state,"mission_revision":"summary-input","runs":details.iter().map(|r|r["id"].clone()).collect::<Vec<_>>(),"run_details":details}))?);
        }
        Ok((result, deadline))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_future_requests_seek_past_historical_claims() {
        let root = tempfile::tempdir().unwrap();
        let store =
            Store::open(&root.path().join("summary-plan.sqlite"), "summary-fixture").unwrap();
        let c = store.readers.get();
        for sql in [FUTURE_ASK_SAME_WIDTH, FUTURE_ASK_LONGER] {
            let plan: Vec<String> = c
                .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
                .unwrap()
                .query_map([now_ms().to_string()], |r| r.get(3))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            assert!(
                plan.iter()
                    .any(|p| p.contains("SEARCH claims USING INDEX claims_accepted_order_index")),
                "future deadline must seek: {plan:?}"
            );
            assert!(
                plan.iter()
                    .all(|p| !p.contains("SCAN claims") && !p.contains("TEMP B-TREE")),
                "no historical scan or sort: {plan:?}"
            );
        }
    }
}
