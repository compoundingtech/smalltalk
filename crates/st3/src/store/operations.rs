//! Bounded operational reads. These never hydrate a run's steps or replay work history.
use super::*;

fn short(value: &str) -> String {
    value.chars().take(2_000).collect()
}

impl Store {
    /// Counts all runs; previews only the newest runs and the newest failures.
    pub fn mission_overview(&self, mission: &str, limit: usize) -> Result<Value> {
        let mission = mission.strip_prefix("mission/").unwrap_or(mission);
        let connection = self.readers.get();
        let mut counts = BTreeMap::<String, u64>::new();
        let mut statement = connection.prepare(
            "SELECT status, COUNT(*) FROM mission_runs WHERE mission_id=?1 GROUP BY status",
        )?;
        for row in statement.query_map([mission], |row| Ok((row.get(0)?, row.get(1)?)))? {
            let (state, count) = row?;
            counts.insert(state, count);
        }
        let mut previews = Vec::new();
        for failed in [false, true] {
            let mut statement = connection.prepare(
                "SELECT r.id, r.current_generation_id, r.requester, r.status, r.phase, r.created_at_unix_ms,
                        r.updated_at_unix_ms,COALESCE(g.revision,r.initial_revision) FROM mission_runs r
                 LEFT JOIN run_generations g ON g.id=r.current_generation_id
                 WHERE r.mission_id=?1 AND (NOT ?2 OR r.status='failed')
                 ORDER BY r.created_at_unix_ms DESC, r.id DESC LIMIT ?3",
            )?;
            let mut items = Vec::new();
            for row in statement.query_map(params![mission, failed, limit.clamp(1, 20)], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(7)?,
                ))
            })? {
                let (id, generation, requester, status, phase, created, updated, revision) = row?;
                let subject = format!("mission-run/{id}");
                let reason: Option<String> = connection
                    .query_row(
                        "SELECT COALESCE(json_extract(body,'$.fields.reason'),
                                     json_extract(body,'$.fields.summary'))
                     FROM claims WHERE subject=?1 AND kind='mission-run.state'
                       AND COALESCE(json_extract(body,'$.fields.reason'),json_extract(body,'$.fields.summary')) IS NOT NULL
                       AND (json_extract(body,'$.fields.status')=?2 OR json_extract(body,'$.fields.phase') LIKE '%-' || ?2)
                     ORDER BY store_index DESC LIMIT 1",
                        params![subject,status],
                        |row| row.get(0),
                    )
                    .optional()?
                    .flatten();
                items.push(
                    json!({"id": subject, "generation_id": format!("run-generation/{generation}"),
                    "requester": short(&requester), "status": status, "phase": phase,
                    "revision":revision,
                    "created_at_unix_ms": created.parse::<u128>()?,
                    "updated_at_unix_ms": updated.parse::<u128>()?,
                    "reason": reason.as_deref().map(short)}),
                );
            }
            previews.push(items);
        }
        Ok(json!({"mission": format!("mission/{mission}"),
            "total_runs": counts.values().sum::<u64>(), "counts": counts,
            "newest": previews[0], "failed": previews[1], "preview_limit": limit.clamp(1,20)}))
    }

    /// A run card's progress counts cover every step; only its first twenty steps are hydrated.
    pub fn mission_step_preview(&self, run: &str) -> Result<(u64, u64, Vec<StepRunView>)> {
        let run = run.trim_start_matches("mission-run/");
        let connection = self.readers.get();
        let (total, done) = connection.query_row(
            "SELECT COUNT(*),COALESCE(SUM(s.status='completed'),0)
             FROM step_runs s JOIN mission_runs r ON r.id=s.run_id
             WHERE r.id=?1 AND s.generation_id=r.current_generation_id",
            [run],
            |row| Ok((row.get::<_, u64>(0)?, row.get::<_, u64>(1)?)),
        )?;
        let mut statement=connection.prepare(
            "SELECT subject,run_id,step_path,definition_hash,status,attempt,assignee,available_to,agentless,title,goals,worker_reported,
                    lease_owner,lease_incarnation,lease_expires_at_unix_ms,blocked_reason,not_before_unix_ms,created_at_unix_ms,updated_at_unix_ms,readiness_epoch,constraints
             FROM step_runs WHERE run_id=?1 AND generation_id=(SELECT current_generation_id FROM mission_runs WHERE id=?1)
             ORDER BY created_at_unix_ms,step_path LIMIT 20",
        )?;
        let mut steps = statement
            .query_map([run], step_run_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        apply_step_states_tx(&connection, &mut steps, false)?;
        Ok((total, done, steps))
    }

    /// Terminal transitions in a stable claim-index window. A retry does not erase a failure.
    pub fn outcome_history(
        &self,
        collection: &str,
        since: u128,
        until: u128,
        status: Option<&str>,
        actor: Option<&str>,
        before: u64,
        limit: usize,
    ) -> Result<Value> {
        anyhow::ensure!(
            matches!(collection, "missions" | "work"),
            "unknown outcome collection"
        );
        anyhow::ensure!(
            status.is_none_or(|s| matches!(s, "failed" | "cancelled" | "timed-out" | "completed")),
            "status must be failed, cancelled, timed-out, or completed"
        );
        let connection = self.readers.get();
        let kinds = if collection == "missions" {
            "c.kind='mission-run.state'"
        } else {
            "c.kind IN ('step-run.state','work.failed')"
        };
        let reason = "COALESCE(json_extract(c.body,'$.fields.reason'), json_extract(c.body,'$.fields.summary'),
            (SELECT COALESCE(json_extract(p.body,'$.fields.reason'),json_extract(p.body,'$.fields.summary'))
             FROM claims p WHERE p.subject=c.subject AND p.kind=c.kind AND p.store_index<c.store_index
               AND COALESCE(json_extract(p.body,'$.fields.reason'),json_extract(p.body,'$.fields.summary')) IS NOT NULL
               AND (json_extract(p.body,'$.fields.status')=json_extract(c.body,'$.fields.status')
                    OR json_extract(p.body,'$.fields.phase') LIKE '%-' || json_extract(c.body,'$.fields.status'))
             ORDER BY p.store_index DESC LIMIT 1))";
        // The kind index bounds this to lifecycle claims even on graphs dominated by heartbeats.
        let sql = format!(
            "SELECT c.store_index, c.id, c.subject, c.actor, c.body, c.accepted_at_unix_ms,
                    COALESCE(r.mission_id,sr.mission_id), s.run_id, {reason}
             FROM claims c LEFT JOIN step_runs s ON s.subject=c.subject
             LEFT JOIN mission_runs sr ON sr.id=s.run_id
             LEFT JOIN mission_runs r ON r.id=substr(c.subject,13) AND c.subject LIKE 'mission-run/%'
             WHERE c.kind IN ('mission-run.state','step-run.state','work.failed')
               AND {kinds} AND c.store_index < ?1
               AND CAST(c.accepted_at_unix_ms AS INTEGER)>=?2
               AND CAST(c.accepted_at_unix_ms AS INTEGER)<=?3
               AND json_extract(c.body,'$.fields.status') IN ('failed','cancelled','completed')
               AND (?4 IS NULL OR json_extract(c.body,'$.fields.status')=?4
                    OR (?4='timed-out' AND json_extract(c.body,'$.fields.status')='failed'
                        AND (lower(COALESCE({reason},'')) LIKE '%timeout%'
                          OR lower(COALESCE({reason},'')) LIKE '%timed out%')))
               AND (?5 IS NULL OR c.actor=?5 OR s.assignee=?5 OR s.lease_owner=?5)
             ORDER BY c.store_index DESC LIMIT ?6"
        );
        let mut statement = connection.prepare(&sql)?;
        let rows = statement.query_map(
            params![
                before.min(i64::MAX as u64),
                since as i64,
                until.min(i64::MAX as u128) as i64,
                status,
                actor,
                limit.clamp(1, 200) + 1
            ],
            |row| {
                Ok((
                    row.get::<_, u64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, Option<String>>(7)?,
                    row.get::<_, Option<String>>(8)?,
                ))
            },
        )?;
        let mut items = Vec::new();
        for row in rows {
            let (index, id, subject, actor, body, at, mission, run, reason) = row?;
            let body: Value = serde_json::from_str(&body)?;
            let fields = &body["fields"];
            items.push(
                json!({"index":index, "id":id, "subject":subject, "actor":actor,
                "status":fields["status"], "reason":reason.as_deref().map(short),
                "at_unix_ms":at.parse::<u128>()?, "mission":mission.map(|m| format!("mission/{m}")),
                "run":run.map(|r| format!("mission-run/{r}"))}),
            );
        }
        let has_more = items.len() > limit.clamp(1, 200);
        items.truncate(limit.clamp(1, 200));
        Ok(
            json!({"collection":collection, "since_unix_ms":since, "until_unix_ms":until,
            "status":status, "items":items, "has_more":has_more,
            "next_before":if has_more { items.last().and_then(|v| v["index"].as_u64()) } else {None}}),
        )
    }
}

#[cfg(test)]
impl Store {
    /// Generated read fixture: 2,401 runs and steps, 250,000 unrelated claims, and terminal
    /// transitions. Projection rows are bulk-loaded to keep fleet-scale tests inexpensive.
    pub(crate) fn seed_operational_fleet(&self) {
        let source = "version 2\nmission \"example/fleet\" state=\"ready\" {\n goal \"Build artifacts.\"\n step \"build\" { goal \"Build the example.\" }\n}\n";
        let intent = crate::graph::parse_intent(source, &self.origin).unwrap();
        let planned = self
            .mission(
                &intent,
                IntentInput {
                    kdl: source.into(),
                    source_name: None,
                },
            )
            .unwrap();
        self.apply(&intent, &planned.subject_tokens, "fixture-mission")
            .unwrap();
        let run = self
            .create_mission_run(&MissionRunRequest {
                mission: "example/fleet".into(),
                revision: None,
                workspace: "/example".into(),
                requester: Some("person/operator".into()),
                mode: None,
                inputs: BTreeMap::new(),
                idempotency_key: "fixture-run".into(),
            })
            .unwrap();
        let mut connection = self.connection.lock().unwrap();
        let transaction = connection.transaction().unwrap();
        let batch: String = transaction
            .query_row("SELECT batch_id FROM claims LIMIT 1", [], |r| r.get(0))
            .unwrap();
        let at = now_ms().to_string();
        transaction.execute(
            "WITH RECURSIVE seq(n) AS (VALUES(0) UNION ALL SELECT n+1 FROM seq WHERE n<249999)
             INSERT INTO claims(id,batch_id,subject,kind,origin,body,predecessors,accepted_at_unix_ms)
             SELECT 'fixture-heartbeat-' || n,?1,'agent/example/observer','harness.observed',
                    'fixture',?2,'[]',?3 FROM seq",
            params![batch,r#"{"fields":{"state":"idle","incarnation_id":"fixture"}}"#,at],
        ).unwrap();
        for i in 0..2400 {
            let id = format!("example/fleet/seed-{i:05}");
            let generation = format!("fixture-generation-{i}");
            let status = ["running", "completed", "failed", "cancelled"][i % 4];
            let phase = if status == "running" {
                "normal"
            } else {
                "terminal"
            };
            transaction.execute("INSERT INTO mission_runs SELECT ?1,mission_id,initial_revision,?2,root_revision,?1,NULL,workspace,requester,inputs,mode,?3,?4,?5,?5 FROM mission_runs WHERE id=?6",
                params![id,generation,status,phase,at,run.id]).unwrap();
            transaction.execute("INSERT INTO run_generations SELECT ?1,?2,revision,NULL,?3,actor,reason,?4,?4 FROM run_generations WHERE id=?5",
                params![generation,id,status,at,run.generation.trim_start_matches("run-generation/")]).unwrap();
            let step = format!("step-run/{generation}/build");
            transaction.execute("INSERT INTO step_runs(subject,run_id,generation_id,step_path,definition_hash,status,attempt,assignee,available_to,agentless,title,goals,created_at_unix_ms,updated_at_unix_ms)
                SELECT ?1,?2,?3,step_path,definition_hash,?4,attempt,'agent/example/builder',available_to,agentless,title,goals,?5,?5 FROM step_runs WHERE subject=?6",
                params![step,id,generation,status,at,run.steps[0].subject]).unwrap();
            let reason = if i % 8 == 2 {
                "the active execution timeout expired"
            } else {
                "the example check failed"
            };
            for (subject, kind) in [
                (format!("mission-run/{id}"), "mission-run.state"),
                (step, "step-run.state"),
            ] {
                transaction.execute("INSERT INTO claims(id,batch_id,subject,kind,origin,body,predecessors,accepted_at_unix_ms) VALUES (?1,?2,?3,?4,'fixture',?5,'[]',?6)",
                    params![format!("fixture-{kind}-{i}"),batch,subject,kind,json!({"fields":{"status":status,"phase":phase,"reason":reason}}).to_string(),at]).unwrap();
            }
        }
        transaction.commit().unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fleet_overview_counts_all_runs_but_bounds_previews() {
        let store = Store::open_memory("fixture").unwrap();
        store.seed_operational_fleet();
        let overview = store.mission_overview("mission/example/fleet", 10).unwrap();
        assert_eq!(overview["total_runs"], 2401);
        assert_eq!(overview["counts"]["running"], 601);
        assert_eq!(overview["counts"]["failed"], 600);
        assert_eq!(overview["newest"].as_array().unwrap().len(), 10);
        assert_eq!(overview["failed"].as_array().unwrap().len(), 10);
        assert!(serde_json::to_vec(&overview).unwrap().len() < 20_000);
        assert_eq!(
            store
                .mission_overview("mission/example/unstarted", 10)
                .unwrap()["total_runs"],
            0
        );
    }
    #[test]
    fn fleet_outcomes_page_stably_keep_reasons_and_filter_time_status_and_actor() {
        let store = Store::open_memory("fixture").unwrap();
        store.seed_operational_fleet();
        for collection in ["missions", "work"] {
            let first = store
                .outcome_history(collection, 0, now_ms(), Some("failed"), None, u64::MAX, 50)
                .unwrap();
            assert_eq!(first["items"].as_array().unwrap().len(), 50);
            assert_eq!(first["has_more"], true);
            assert!(
                first["items"][0]["reason"]
                    .as_str()
                    .unwrap()
                    .contains("failed")
            );
            let before = first["next_before"].as_u64().unwrap();
            let second = store
                .outcome_history(collection, 0, now_ms(), Some("failed"), None, before, 50)
                .unwrap();
            assert!(second["items"][0]["index"].as_u64().unwrap() < before);
            let timed = store
                .outcome_history(
                    collection,
                    0,
                    now_ms(),
                    Some("timed-out"),
                    None,
                    u64::MAX,
                    50,
                )
                .unwrap();
            assert!(
                timed["items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|item| item["reason"].as_str().unwrap().contains("timeout"))
            );
            let cancelled = store
                .outcome_history(
                    collection,
                    0,
                    now_ms(),
                    Some("cancelled"),
                    None,
                    u64::MAX,
                    50,
                )
                .unwrap();
            assert_eq!(cancelled["items"].as_array().unwrap().len(), 50);
            let old = store
                .outcome_history(collection, 0, 1, None, None, u64::MAX, 50)
                .unwrap();
            assert!(old["items"].as_array().unwrap().is_empty());
        }
        let actor = store
            .outcome_history(
                "work",
                0,
                now_ms(),
                Some("failed"),
                Some("agent/example/builder"),
                u64::MAX,
                50,
            )
            .unwrap();
        assert_eq!(actor["items"].as_array().unwrap().len(), 50);
        let other = store
            .outcome_history(
                "work",
                0,
                now_ms(),
                Some("failed"),
                Some("agent/example/other"),
                u64::MAX,
                50,
            )
            .unwrap();
        assert!(other["items"].as_array().unwrap().is_empty());
        // A retry changes the current row, while the earlier terminal transition remains.
        store.connection.lock().unwrap().execute("UPDATE step_runs SET status='ready' WHERE subject='step-run/fixture-generation-2398/build'",[]).unwrap();
        let history = store
            .outcome_history("work", 0, now_ms(), Some("failed"), None, u64::MAX, 1)
            .unwrap();
        assert_eq!(
            history["items"][0]["subject"],
            "step-run/fixture-generation-2398/build"
        );
        assert_eq!(history["items"][0]["status"], "failed");
    }
}
