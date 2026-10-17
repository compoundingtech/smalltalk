use super::*;

impl Store {
    /// Whether a refresher keeps the roster published, so readers must never fold it.
    pub fn agent_roster_refresher_running(&self) -> bool {
        self.smalltalk.agent_roster_refresh.get().is_some()
    }

    /// Rebuild only the planning tables. A planning claim written through the generic claim path
    /// is projected by this rebuild alone; the other projections already took it at the append,
    /// and rebuilding all of them re-derived every operation (about 240,000 rows) under the
    /// writer for each planning write.
    pub fn rebuild_planning_projection(&self) -> Result<()> {
        let mut connection = self.connection.write();
        let transaction = connection.transaction()?;
        rebuild_planning_tx(&transaction)?;
        transaction.commit()?;
        Ok(())
    }

    pub fn mission_provenance(
        &self,
        mission: &str,
        revision: &str,
    ) -> Result<Option<crate::provenance::Provenance>> {
        crate::provenance::read(&self.readers.get(), mission, revision)
    }

    pub fn mission_spec(
        &self,
        mission_id: &str,
        revision: Option<&str>,
    ) -> Result<Option<MissionSpec>> {
        smallclaims::touched::note_read(|| format!("mission/{mission_id}"));
        let connection = self.readers.get();
        let body = if let Some(revision) = revision {
            connection
                .query_row(
                    "SELECT body FROM mission_revisions WHERE mission_id=?1 AND revision=?2",
                    params![mission_id, revision],
                    |row| row.get::<_, String>(0),
                )
                .optional()?
        } else {
            connection
                .query_row(
                    "SELECT r.body FROM mission_definitions d JOIN mission_revisions r ON r.mission_id=d.mission_id AND r.revision=d.revision WHERE d.mission_id=?1",
                    [mission_id],
                    |row| row.get::<_, String>(0),
                )
                .optional()?
        };
        body.map(|body| serde_json::from_str(&body))
            .transpose()
            .map_err(Into::into)
    }

    /// Load the definitions behind a collection of runs in one read.
    pub fn mission_specs_for_runs(&self, runs: &[String]) -> Result<BTreeMap<String, MissionSpec>> {
        if runs.is_empty() {
            return Ok(BTreeMap::new());
        }
        let ids = runs
            .iter()
            .map(|run| run.strip_prefix("mission-run/").unwrap_or(run))
            .collect::<Vec<_>>();
        let connection = self.readers.get();
        let mut statement = connection.prepare(
            "SELECT mission_runs.id, mission_revisions.body
             FROM mission_runs
             JOIN run_generations ON run_generations.id=mission_runs.current_generation_id
             JOIN mission_revisions ON mission_revisions.mission_id=mission_runs.mission_id
              AND mission_revisions.revision=run_generations.revision
             WHERE mission_runs.id IN (SELECT value FROM json_each(?1))",
        )?;
        statement
            .query_map([serde_json::to_string(&ids)?], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .map(|row| {
                let (id, body) = row?;
                Ok((format!("mission-run/{id}"), serde_json::from_str(&body)?))
            })
            .collect()
    }

    /// Name steps for display in one read: each step's mission, run, path, title, first goal,
    /// status and last change. Unknown subjects are left out.
    pub fn step_labels(&self, subjects: &[String]) -> Result<BTreeMap<String, StepLabel>> {
        if subjects.is_empty() {
            return Ok(BTreeMap::new());
        }
        let connection = self.readers.get();
        step_labels::read(&connection, subjects)
    }

    /// The mission behind each of these runs, in one read.
    pub fn run_missions(&self, runs: &[String]) -> Result<BTreeMap<String, String>> {
        if runs.is_empty() {
            return Ok(BTreeMap::new());
        }
        let ids = runs
            .iter()
            .map(|run| run.strip_prefix("mission-run/").unwrap_or(run))
            .collect::<Vec<_>>();
        let connection = self.readers.get();
        let mut statement = connection.prepare(
            "SELECT id, mission_id FROM mission_runs WHERE id IN (SELECT value FROM json_each(?1))",
        )?;
        statement
            .query_map([serde_json::to_string(&ids)?], |row| {
                Ok((
                    format!("mission-run/{}", row.get::<_, String>(0)?),
                    format!("mission/{}", row.get::<_, String>(1)?),
                ))
            })?
            .collect::<rusqlite::Result<_>>()
            .map_err(Into::into)
    }

    /// Return every current published mission definition, including definitions with no runs.
    pub fn mission_definitions(&self) -> Result<Vec<MissionDefinitionView>> {
        let connection = self.readers.get();
        let mut statement = connection.prepare(
            "SELECT r.body, c.accepted_at_unix_ms
             FROM mission_definitions d
             JOIN mission_revisions r
               ON r.mission_id=d.mission_id AND r.revision=d.revision
             JOIN claims c ON c.id=d.claim_id
             ORDER BY d.mission_id",
        )?;
        statement
            .query_map([], |row| {
                let body = row.get::<_, String>(0)?;
                let accepted = row.get::<_, String>(1)?;
                Ok((body, accepted))
            })?
            .map(|row| {
                let (body, accepted) = row?;
                Ok(MissionDefinitionView {
                    mission: serde_json::from_str(&body)?,
                    updated_at_unix_ms: accepted.parse()?,
                })
            })
            .collect()
    }

    /// The definitions of the missions that have no run, as `mission_definitions` reads them,
    /// without reading any run or the definitions of missions that ran.
    pub fn mission_definitions_without_runs(&self) -> Result<Vec<MissionDefinitionView>> {
        let connection = self.readers.get();
        let mut statement = connection.prepare_cached(
            "SELECT r.body, c.accepted_at_unix_ms
             FROM mission_definitions d
             JOIN mission_revisions r
               ON r.mission_id=d.mission_id AND r.revision=d.revision
             JOIN claims c ON c.id=d.claim_id
             WHERE NOT EXISTS (SELECT 1 FROM mission_runs WHERE mission_runs.mission_id=d.mission_id)
             ORDER BY d.mission_id",
        )?;
        statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .map(|row| {
                let (body, accepted) = row?;
                Ok(MissionDefinitionView {
                    mission: serde_json::from_str(&body)?,
                    updated_at_unix_ms: accepted.parse()?,
                })
            })
            .collect()
    }

    /// Ordered mission IDs for one bounded collection page. Selection and
    /// history filtering happen in SQL before resource joins are hydrated.
    pub fn mission_collection_ids(
        &self,
        history: bool,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<String>> {
        self.mission_collection_ids_at(history, offset, limit, now_ms())
    }

    pub fn mission_collection_ids_at(
        &self,
        history: bool,
        offset: usize,
        limit: usize,
        at_unix_ms: u128,
    ) -> Result<Vec<String>> {
        Ok(self
            .mission_collection_page_at(history, offset, limit, None, at_unix_ms)?
            .into_iter()
            .map(|(id, _)| id)
            .collect())
    }

    pub fn mission_collection_page(
        &self,
        history: bool,
        offset: usize,
        limit: usize,
        after: Option<&(u128, String)>,
    ) -> Result<Vec<(String, u128)>> {
        self.mission_collection_page_at(history, offset, limit, after, now_ms())
    }

    pub fn mission_collection_page_at(
        &self,
        history: bool,
        offset: usize,
        limit: usize,
        after: Option<&(u128, String)>,
        at_unix_ms: u128,
    ) -> Result<Vec<(String, u128)>> {
        let ended_since = at_unix_ms.saturating_sub(RECENTLY_ENDED_MS);
        let connection = self.readers.get();
        let mut statement = connection.prepare(
            "WITH ids AS (
                SELECT mission_id FROM mission_definitions
                UNION SELECT mission_id FROM mission_runs
             ), run_states AS (
                SELECT mission_id, SUM(status='running') AS running, SUM(status='standing') AS standing
                FROM mission_runs GROUP BY mission_id
             ), latest AS (
                SELECT mission_id, status, updated_at_unix_ms,
                       ROW_NUMBER() OVER (PARTITION BY mission_id ORDER BY created_at_unix_ms DESC, id DESC) AS rank
                FROM mission_runs
             )
             SELECT ids.mission_id, COALESCE(latest.updated_at_unix_ms,published.accepted_at_unix_ms,'0')
             FROM ids
             LEFT JOIN mission_definitions def ON def.mission_id=ids.mission_id
             LEFT JOIN claims published ON published.id=def.claim_id
             LEFT JOIN run_states ON run_states.mission_id=ids.mission_id
             LEFT JOIN latest ON latest.mission_id=ids.mission_id AND latest.rank=1
             WHERE (?5 IS NULL OR CAST(COALESCE(latest.updated_at_unix_ms,published.accepted_at_unix_ms, '0') AS INTEGER) < ?5
                 OR (CAST(COALESCE(latest.updated_at_unix_ms,published.accepted_at_unix_ms, '0') AS INTEGER) = ?5 AND ids.mission_id > ?6))
               AND (?1 OR ids.mission_id NOT LIKE '__st3/%')
               AND (?1 OR CASE
                   WHEN COALESCE(run_states.running,0)>0 THEN 'running'
                   WHEN COALESCE(run_states.standing,0)>0 THEN 'standing'
                   WHEN def.state='retired' THEN 'retired'
                   WHEN latest.status IS NOT NULL THEN latest.status
                   ELSE def.state END NOT IN ('completed','failed','cancelled','retired')
                   -- A run that failed or was cancelled stays in view for a while with its
                   -- outcome, instead of vanishing the moment it ends, unless its mission
                   -- was retired.
                   OR (latest.status IN ('failed','cancelled')
                       AND COALESCE(def.state,'')<>'retired'
                       AND COALESCE(run_states.running,0)=0 AND COALESCE(run_states.standing,0)=0
                       AND CAST(latest.updated_at_unix_ms AS INTEGER)>=?4))
             ORDER BY CAST(COALESCE(latest.updated_at_unix_ms,published.accepted_at_unix_ms) AS INTEGER) DESC,
                      ids.mission_id ASC
             LIMIT ?2 OFFSET ?3",
        )?;
        statement
            .query_map(
                params![
                    history,
                    limit as i64,
                    offset as i64,
                    ended_since as i64,
                    after.map(|key| key.0 as i64),
                    after.map(|key| key.1.trim_start_matches("mission/"))
                ],
                |row| {
                    Ok((
                        format!("mission/{}", row.get::<_, String>(0)?),
                        row.get::<_, String>(1)?,
                    ))
                },
            )?
            .map(|row| {
                let (id, time) = row?;
                Ok((id, time.parse()?))
            })
            .collect()
    }

    pub fn mission_definitions_for_ids(
        &self,
        ids: &[String],
    ) -> Result<Vec<MissionDefinitionView>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let connection = self.readers.get();
        let placeholders = vec!["?"; ids.len()].join(",");
        let sql = format!(
            "SELECT r.body, c.accepted_at_unix_ms FROM mission_definitions d
             JOIN mission_revisions r ON r.mission_id=d.mission_id AND r.revision=d.revision
             JOIN claims c ON c.id=d.claim_id WHERE d.mission_id IN ({placeholders})"
        );
        let bare = ids.iter().map(|id| id.trim_start_matches("mission/"));
        let mut statement = connection.prepare(&sql)?;
        statement
            .query_map(rusqlite::params_from_iter(bare), |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .map(|row| {
                let (body, accepted) = row?;
                Ok(MissionDefinitionView {
                    mission: serde_json::from_str(&body)?,
                    updated_at_unix_ms: accepted.parse()?,
                })
            })
            .collect()
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_planning_session(
        &self,
        id: &str,
        mission: &str,
        request_ref: &str,
        workspace: &str,
        requester: &str,
        planner: &str,
        planner_config: &PlannerSpec,
        target_run: Option<&str>,
        source_generation: Option<&str>,
    ) -> Result<PlanningSessionView, St3Error> {
        let now = now_ms().to_string();
        let connection = self.connection.write();
        connection
            .execute(
                "INSERT OR IGNORE INTO planning_sessions(id, mission_id, request_ref, workspace, requester, planner, planner_spec_json, status, target_run_id, source_generation_id, created_at_unix_ms, updated_at_unix_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'planning', ?8, ?9, ?10, ?10)",
                params![id, mission, request_ref, workspace, requester, planner, serde_json::to_string(planner_config).map_err(internal)?, target_run.map(|run| run.strip_prefix("mission-run/").unwrap_or(run)), source_generation.map(generation_id_from_subject), now],
            )
            .map_err(internal)?;
        planning_session_view_tx(&connection, id)
            .map_err(internal)?
            .ok_or_else(|| St3Error::new("missing-launch", "the launch was not stored"))
    }

    pub fn planning_session(&self, id: &str) -> Result<Option<PlanningSessionView>> {
        let id = id.strip_prefix("planning-session/").unwrap_or(id);
        let connection = self.readers.get();
        planning_session_view_tx(&connection, id)
    }

    pub fn planning_sessions(&self, include_history: bool) -> Result<Vec<PlanningSessionView>> {
        let connection = self.readers.get();
        let mut statement = connection.prepare(
            "SELECT id FROM planning_sessions
             WHERE ?1 OR status NOT IN ('approved','cancelled','failed')
             ORDER BY updated_at_unix_ms DESC, id",
        )?;
        let ids = statement
            .query_map([include_history], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        ids.into_iter()
            .filter_map(|id| planning_session_view_tx(&connection, &id).transpose())
            .collect()
    }

    /// The launches of one mission, newest first as `planning_sessions(true)` orders them, with
    /// or without its `mission/` prefix, read without every other mission's launches.
    pub fn planning_sessions_for_mission(&self, mission: &str) -> Result<Vec<PlanningSessionView>> {
        let connection = self.readers.get();
        let mut statement = connection.prepare_cached(
            "SELECT id FROM planning_sessions
             WHERE mission_id IN (?1, ?2, ?3)
             ORDER BY updated_at_unix_ms DESC, id",
        )?;
        let ids = statement
            .query_map(
                params![
                    mission,
                    mission.trim_start_matches("mission/"),
                    mission.strip_prefix("mission/").unwrap_or(mission)
                ],
                |row| row.get::<_, String>(0),
            )?
            .collect::<Result<Vec<_>, _>>()?;
        ids.into_iter()
            .filter_map(|id| planning_session_view_tx(&connection, &id).transpose())
            .collect()
    }

    pub fn add_planning_candidate(
        &self,
        id: &str,
        actor: &str,
        variant: &str,
        markdown_ref: &str,
        kdl_ref: &str,
        mission_revision: &str,
    ) -> Result<PlanningSessionView, St3Error> {
        let id = id.strip_prefix("planning-session/").unwrap_or(id);
        let mut connection = self.connection.write();
        let transaction = connection.transaction().map_err(internal)?;
        let (planner, status): (String, String) = transaction
            .query_row(
                "SELECT planner, status FROM planning_sessions WHERE id=?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(internal)?
            .ok_or_else(|| {
                St3Error::new("missing-launch", format!("launch `{id}` does not exist"))
            })?;
        if planner != normalize_actor(actor, "agent") {
            return Err(St3Error::new(
                "wrong-planner",
                format!("`{actor}` does not own launch `{id}`"),
            ));
        }
        if !matches!(
            status.as_str(),
            "planning" | "revision-requested" | "review"
        ) {
            return Err(St3Error::new(
                "launch-terminal",
                format!("launch `{id}` is {status}"),
            ));
        }
        let current = transaction
            .query_row(
                "SELECT markdown_ref, kdl_ref, mission_revision FROM planning_candidates
                 WHERE session_id=?1 AND variant=?2 ORDER BY revision DESC LIMIT 1",
                params![id, variant],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(internal)?;
        if status == "review"
            && current.as_ref().is_some_and(|(markdown, kdl, revision)| {
                markdown == markdown_ref && kdl == kdl_ref && revision == mission_revision
            })
        {
            transaction.commit().map_err(internal)?;
            drop(connection);
            return self
                .planning_session(id)
                .map_err(internal)?
                .ok_or_else(|| St3Error::new("missing-launch", "the launch disappeared"));
        }
        let revision: u32 = transaction
            .query_row(
                "SELECT COALESCE(MAX(revision), 0) + 1 FROM planning_candidates WHERE session_id=?1 AND variant=?2",
                params![id, variant],
                |row| row.get(0),
            )
            .map_err(internal)?;
        let now = now_ms().to_string();
        transaction
            .execute(
                "INSERT INTO planning_candidates(session_id, variant, revision, markdown_ref, kdl_ref, mission_revision, submitted_at_unix_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![id, variant, revision, markdown_ref, kdl_ref, mission_revision, now],
            )
            .map_err(internal)?;
        transaction
            .execute(
                "DELETE FROM planning_previews WHERE session_id=?1 AND variant=?2",
                params![id, variant],
            )
            .map_err(internal)?;
        transaction
            .execute(
                "UPDATE planning_sessions SET status='review', updated_at_unix_ms=?2 WHERE id=?1",
                params![id, now],
            )
            .map_err(internal)?;
        transaction.commit().map_err(internal)?;
        drop(connection);
        self.planning_session(id)
            .map_err(internal)?
            .ok_or_else(|| St3Error::new("missing-launch", "the launch disappeared"))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn save_planning_preview(
        &self,
        id: &str,
        variant: &str,
        candidate_revision: u32,
        hash: &str,
        graph: &str,
        diff: &str,
        mission: &MissionResponse,
    ) -> Result<PlanningSessionView, St3Error> {
        let id = id.strip_prefix("planning-session/").unwrap_or(id);
        let now = now_ms().to_string();
        let connection = self.connection.write();
        connection
            .execute(
                "INSERT INTO planning_previews(session_id, variant, candidate_revision, hash, store_index, graph, diff, mission_response, created_at_unix_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT(session_id, variant) DO UPDATE SET candidate_revision=excluded.candidate_revision, hash=excluded.hash,
                   store_index=excluded.store_index, graph=excluded.graph, diff=excluded.diff,
                   mission_response=excluded.mission_response, created_at_unix_ms=excluded.created_at_unix_ms",
                params![
                    id,
                    variant,
                    candidate_revision,
                    hash,
                    mission.store_index,
                    graph,
                    diff,
                    serde_json::to_string(mission).map_err(internal)?,
                    now,
                ],
            )
            .map_err(internal)?;
        planning_session_view_tx(&connection, id)
            .map_err(internal)?
            .ok_or_else(|| St3Error::new("missing-launch", format!("launch `{id}` does not exist")))
    }

    pub fn request_planning_revision(
        &self,
        id: &str,
        actor: &str,
    ) -> Result<PlanningSessionView, St3Error> {
        self.set_planning_status(id, actor, "revision-requested", None)
    }

    pub fn finish_planning_session(
        &self,
        id: &str,
        actor: &str,
        status: &str,
        published_revision: Option<&str>,
    ) -> Result<PlanningSessionView, St3Error> {
        if !matches!(status, "approved" | "cancelled") {
            return Err(St3Error::new(
                "invalid-launch-status",
                "invalid terminal planning status",
            ));
        }
        self.set_planning_status(id, actor, status, published_revision)
    }

    pub fn set_planning_status(
        &self,
        id: &str,
        actor: &str,
        status: &str,
        published_revision: Option<&str>,
    ) -> Result<PlanningSessionView, St3Error> {
        let id = id.strip_prefix("planning-session/").unwrap_or(id);
        let connection = self.connection.write();
        let (requester, current): (String, String) = connection
            .query_row(
                "SELECT requester, status FROM planning_sessions WHERE id=?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(internal)?
            .ok_or_else(|| {
                St3Error::new("missing-launch", format!("launch `{id}` does not exist"))
            })?;
        if requester != normalize_actor(actor, "person") {
            return Err(St3Error::new(
                "launch-review-not-authorized",
                format!("`{actor}` cannot review launch `{id}`"),
            ));
        }
        let transition_is_valid = match status {
            "revision-requested" => current == "review",
            "approved" => current == "review" || current == "approved",
            "cancelled" => {
                matches!(
                    current.as_str(),
                    "planning" | "review" | "revision-requested" | "cancelled"
                )
            }
            _ => false,
        };
        if !transition_is_valid {
            return Err(St3Error::new(
                "invalid-launch-transition",
                format!("launch `{id}` cannot move from {current} to {status}"),
            ));
        }
        if current == status {
            return planning_session_view_tx(&connection, id)
                .map_err(internal)?
                .ok_or_else(|| St3Error::new("missing-launch", "the launch disappeared"));
        }
        connection
            .execute(
                "UPDATE planning_sessions SET status=?2, published_revision=?3, updated_at_unix_ms=?4 WHERE id=?1",
                params![id, status, published_revision, now_ms().to_string()],
            )
            .map_err(internal)?;
        if status == "revision-requested" {
            connection
                .execute("DELETE FROM planning_previews WHERE session_id=?1", [id])
                .map_err(internal)?;
        }
        planning_session_view_tx(&connection, id)
            .map_err(internal)?
            .ok_or_else(|| St3Error::new("missing-launch", "the launch disappeared"))
    }

    pub fn create_mission_run(
        &self,
        request: &MissionRunRequest,
    ) -> Result<MissionRunView, St3Error> {
        self.create_mission_run_inner(request, None, None, None)
    }

    pub fn mission_run_subject_for_idempotency_key(&self, idempotency_key: &str) -> String {
        format!(
            "mission-run/{}",
            &hex::encode(Sha256::digest(
                format!("{}:{idempotency_key}", self.origin).as_bytes()
            ))[..32]
        )
    }

    pub fn create_child_mission_run(
        &self,
        request: &MissionRunRequest,
        parent: &MissionRunView,
        parent_step_run: &str,
        default_selector: Option<&WorkSelector>,
    ) -> Result<MissionRunView, St3Error> {
        self.create_mission_run_inner(
            request,
            Some(ChildMissionContext {
                root_revision: parent.root_revision.clone(),
                root_run_id: parent
                    .root_mission_run
                    .strip_prefix("mission-run/")
                    .unwrap_or(&parent.root_mission_run)
                    .to_owned(),
                parent_step_run: normalize_step_run(parent_step_run),
                default_selector: default_selector.cloned(),
            }),
            None,
            None,
        )
    }

    /// An occurrence belongs to its schedule, regardless of the requesting member or revision.
    pub fn scheduled_mission_run_subject(schedule: &str, occurrence: u64) -> String {
        format!(
            "mission-run/{}",
            &canonical_hash(&("schedule-occurrence", schedule, occurrence))
                .expect("schedule occurrence identity serializes")[..32]
        )
    }

    pub fn create_scheduled_mission_run(
        &self,
        request: &MissionRunRequest,
        parent: Option<&MissionRunView>,
        schedule: &str,
        occurrence: u64,
    ) -> Result<MissionRunView, St3Error> {
        let subject = Self::scheduled_mission_run_subject(schedule, occurrence);
        // A replicated or replayed run remains the occurrence's run even if the child head,
        // workspace or parent revision has changed since its request was accepted.
        if let Some(run) = self.mission_run(&subject).map_err(internal)? {
            return Ok(run);
        }
        let child = parent.map(|parent| ChildMissionContext {
            root_revision: parent.root_revision.clone(),
            root_run_id: parent
                .root_mission_run
                .trim_start_matches("mission-run/")
                .into(),
            parent_step_run: normalize_step_run(schedule),
            default_selector: None,
        });
        self.create_mission_run_inner(request, child, Some(&subject), None)
    }

    pub fn create_mission_run_inner(
        &self,
        request: &MissionRunRequest,
        child: Option<ChildMissionContext>,
        occurrence_subject: Option<&str>,
        latest_ref: Option<(&str, &str)>,
    ) -> Result<MissionRunView, St3Error> {
        let mission_id = request
            .mission
            .strip_prefix("mission/")
            .unwrap_or(&request.mission);
        if child.is_none() && mission_id.starts_with("__st3/") {
            return Err(St3Error::new(
                "internal-mission",
                "an embedded loop mission can start only through its parent loop",
            ));
        }
        owned_sets::guard_mission_start(&self.readers.get(), mission_id)?;
        let mission = self
            .mission_spec(mission_id, request.revision.as_deref())
            .map_err(internal)?
            .ok_or_else(|| {
                St3Error::new(
                    "missing-mission",
                    format!("mission `{mission_id}` does not exist"),
                )
            })?;
        if mission.state == MissionState::Retired {
            return Err(St3Error::new(
                "mission-retired",
                format!(
                    "mission `{mission_id}` is retired; publish a ready revision to start it again"
                ),
            ));
        }
        if mission.state != MissionState::Ready {
            return Err(St3Error::new(
                "mission-not-ready",
                format!("mission `{mission_id}` is not ready"),
            ));
        }
        let request_hash = hex::encode(Sha256::digest(
            serde_json::to_vec(&json!({
                "request": request,
                "parent": child.as_ref().map(|child| json!({
                    "root_revision": child.root_revision,
                    "root_run_id": child.root_run_id,
                    "parent_step_run": child.parent_step_run,
                    "default_selector": child.default_selector,
                })),
            }))
            .map_err(internal)?,
        ));
        let mut connection = self.connection.write();
        if let Some(subject) = occurrence_subject
            && let Some(run) = connection
                .query_row(
                    "SELECT id FROM mission_runs WHERE id=?1",
                    [subject.trim_start_matches("mission-run/")],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(internal)?
        {
            return mission_run_view_tx(&connection, &run).map_err(internal);
        }
        smallclaims::store::idempotency::cached_response(&connection, &request.idempotency_key)?;
        if let Some((response, stored_hash)) = connection
            .query_row(
                "SELECT i.response, r.request_hash FROM idempotency i JOIN mission_run_requests r ON r.operation_id=i.operation_id WHERE i.operation_id=?1",
                [opaque_cache_key(&request.idempotency_key)],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()
            .map_err(internal)?
        {
            if stored_hash != request_hash {
                return Err(St3Error::new(
                    "idempotency-mismatch",
                    "the mission-run idempotency key was used with different input",
                ));
            }
            return serde_json::from_str(&response).map_err(internal);
        }
        let transaction = connection.transaction().map_err(internal)?;
        owned_sets::guard_mission_start(&transaction, mission_id)?;
        if let Some((resource, discovery)) = latest_ref
            && let Some(reason) = stale_ref_request_tx(&transaction, resource, discovery).map_err(internal)? {
            return Err(St3Error::new("stale-ref-head", reason));
        }
        let inputs = resolve_mission_run_inputs(&transaction, &mission, &request.inputs)?;
        if let Some((resource, discovery)) = latest_ref {
            if let Some(reason) = stale_pull_request_request_tx(&transaction, resource, discovery)
                .map_err(internal)?
            {
                return Err(St3Error::new("stale-pull-request", reason));
            }
            let is_pull_request = claim_by_id_tx(&transaction, discovery)
                .map_err(internal)?
                .is_some_and(|claim| {
                    claim.subject == resource
                        && claim.body.pointer("/fields/kind").and_then(Value::as_str)
                            == Some("vcs.pull-request")
                });
            if is_pull_request {
                let completed: Option<String> = transaction
                    .query_row(
                        "SELECT id FROM mission_runs WHERE mission_id=?1 AND status='completed'
                     AND EXISTS(SELECT 1 FROM json_each(mission_runs.inputs) AS input
                         WHERE json_extract(input.value, '$.subject')=?2
                         AND json_extract(input.value, '$.claim_id')=?3)
                     ORDER BY id LIMIT 1",
                        params![mission_id, resource, discovery],
                        |row| row.get(0),
                    )
                    .optional()
                    .map_err(internal)?;
                if let Some(run) = completed {
                    return Err(St3Error::new(
                        "completed-subscription-snapshot",
                        format!("mission-run/{run} already completed for {resource}@{discovery}"),
                    ));
                }
            }
        }
        enforce_mission_run_capacity(&transaction, &mission)?;
        let subject = occurrence_subject.map(str::to_owned).unwrap_or_else(|| {
            self.mission_run_subject_for_idempotency_key(&request.idempotency_key)
        });
        let run_id = subject.trim_start_matches("mission-run/").to_owned();
        let generation_key = occurrence_subject.map_or_else(
            || format!("{}:{}:generation:1", self.origin, request.idempotency_key),
            |subject| format!("{subject}:generation:1"),
        );
        let generation_id = hex::encode(Sha256::digest(generation_key.as_bytes()))[..32].to_owned();
        let generation_subject = format!("run-generation/{generation_id}");
        let root_revision = child
            .as_ref()
            .map(|child| child.root_revision.clone())
            .unwrap_or_else(|| mission.revision.clone());
        let root_run_id = child
            .as_ref()
            .map(|child| child.root_run_id.clone())
            .unwrap_or_else(|| run_id.clone());
        let root_mission_run = format!("mission-run/{root_run_id}");
        let parent_step_run = child.as_ref().map(|child| child.parent_step_run.clone());
        let default_selector = child
            .as_ref()
            .and_then(|child| child.default_selector.clone());
        let requester = normalize_actor(
            request.requester.as_deref().unwrap_or("person/requester"),
            "person",
        );
        let mode = request.mode.as_deref().unwrap_or("run");
        if !matches!(mode, "run" | "eval") {
            return Err(St3Error::new(
                "invalid-run-mode",
                format!("run mode `{mode}` is not registered"),
            ));
        }
        validate_mission_run_timeout(&mission, mode)?;
        let mut variables = BTreeMap::from([
            ("ST_MISSION".into(), mission.id.clone()),
            ("ST_MISSION_REVISION".into(), mission.revision.clone()),
            ("ST_MISSION_RUN".into(), run_id.clone()),
            ("ST_RUN_GENERATION".into(), generation_id.clone()),
            ("ST_REQUESTER".into(), requester.clone()),
            ("ST_ROOT_MISSION_RUN".into(), root_mission_run.clone()),
            (
                "ST_ROOT_MISSION_RUN_ID".into(),
                root_mission_run
                    .strip_prefix("mission-run/")
                    .unwrap_or(&root_mission_run)
                    .into(),
            ),
            ("ST_WORKSPACE".into(), request.workspace.clone()),
            (
                "ST_PARENT_STEP_RUN".into(),
                parent_step_run.clone().unwrap_or_default(),
            ),
        ]);
        for (name, input) in &inputs {
            variables.insert(format!("input.{name}"), input.value.clone());
        }
        extend_loop_variables(&mut variables, &inputs);
        let now = now_ms();
        transaction
            .execute(
                "INSERT INTO mission_runs(id, mission_id, initial_revision, current_generation_id, root_revision, root_run_id, parent_step_run, workspace, requester, inputs, mode, status, phase, created_at_unix_ms, updated_at_unix_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 'running', 'normal', ?12, ?12)",
                params![run_id, mission.id, mission.revision, generation_id, root_revision, root_run_id, parent_step_run, request.workspace, requester, serde_json::to_string(&inputs).map_err(internal)?, mode, now.to_string()],
            )
            .map_err(internal)?;
        let deadline_at_unix_ms =
            insert_mission_deadline_tx(&transaction, &run_id, mission.timeout_ms, Some(now))?;
        transaction
            .execute(
                "INSERT INTO run_generations(id, run_id, revision, predecessor_id, status, actor, reason, created_at_unix_ms, updated_at_unix_ms)
                 VALUES (?1, ?2, ?3, NULL, 'running', ?4, 'initial mission run', ?5, ?5)",
                params![generation_id, run_id, mission.revision, requester, now.to_string()],
            )
            .map_err(internal)?;
        let mut flat = Vec::new();
        flatten_steps(&mission, default_selector.clone(), &[], &mut flat);
        let parents = crate::mission::parent_step_index(&mission);
        for (step, selector, constraints) in flat {
            let (assignee, available_to, agentless) = interpolate_selector(&selector, &variables)?;
            let step_subject = format!("step-run/{generation_id}/{}", step.path);
            let mut step_variables = variables.clone();
            step_variables.insert("ST_STEP".into(), step.path.clone());
            step_variables.insert("ST_STEP_RUN".into(), step_subject.clone());
            step_variables.insert("ST_ATTEMPT".into(), "1".into());
            step_variables.insert("ST_ASSIGNEE".into(), assignee.clone().unwrap_or_default());
            step_variables.insert(
                "ST_PARENT_STEP_RUN".into(),
                parents
                    .get(step.path.as_str())
                    .copied()
                    .flatten()
                    .map(|path| format!("step-run/{generation_id}/{path}"))
                    .or_else(|| parent_step_run.clone())
                    .unwrap_or_default(),
            );
            let title = step
                .title
                .as_deref()
                .map(|value| crate::mission::interpolate(value, &step_variables))
                .transpose()?;
            let goals = interpolate_goals(&step.goals, &step_variables)?;
            let constraints = interpolate_goals(&constraints, &step_variables)?;
            transaction
                .execute(
                    "INSERT INTO step_runs(subject, run_id, generation_id, step_path, definition_hash, status, attempt, assignee, available_to, agentless, title, goals, created_at_unix_ms, updated_at_unix_ms, constraints)
                     VALUES (?1, ?2, ?3, ?4, ?5, 'pending', 1, ?6, ?7, ?8, ?9, ?10, ?11, ?11, ?12)",
                    params![step_subject, run_id, generation_id, step.path, step.definition_hash, assignee, serde_json::to_string(&available_to).map_err(internal)?, agentless, title, goals, now.to_string(), constraints],
                )
                .map_err(internal)?;
        }
        let body = json!({
            "fields": {
                "status": "running",
                "mission": mission.subject,
                "revision": mission.revision,
                "initial_revision": mission.revision,
                "current_generation": generation_subject,
                "root_revision": root_revision,
                "root_mission_run": root_mission_run,
                "parent_step_run": parent_step_run,
                "default_selector": default_selector,
                "workspace": request.workspace,
                "requester": requester,
                "inputs": inputs,
                "mode": mode,
                "timeout_ms": mission.timeout_ms,
                "deadline_at_unix_ms": deadline_at_unix_ms,
            }
        });
        let mut body = body;
        add_run_report_fields(&mut body, &mission, None, false);
        append_receipt_claim_tx(
            &transaction,
            &self.origin,
            &subject,
            "mission-run.created",
            Some(&requester),
            &body,
            &[],
            None,
            &request.idempotency_key,
        )
        .map_err(internal)?;
        append_claim_tx(
            &transaction,
            &self.origin,
            &generation_subject,
            "run-generation.created",
            Some(&requester),
            &json!({"fields": {
                "run": subject,
                "revision": mission.revision,
                "status": "running",
                "reason": "initial mission run"
            }}),
            &[],
            None,
        )
        .map_err(internal)?;
        let view = mission_run_view_tx(&transaction, &run_id).map_err(internal)?;
        transaction
            .execute(
                "INSERT INTO idempotency(operation_id, response) VALUES (?1, ?2)",
                params![
                    opaque_cache_key(&request.idempotency_key),
                    serde_json::to_string(&view).map_err(internal)?
                ],
            )
            .map_err(internal)?;
        transaction
            .execute(
                "INSERT INTO mission_run_requests(operation_id, request_hash) VALUES (?1, ?2)",
                params![opaque_cache_key(&request.idempotency_key), request_hash],
            )
            .map_err(internal)?;
        transaction.commit().map_err(internal)?;
        Ok(view)
    }

    pub fn record_mission_output(
        &self,
        subject: &str,
        actor: &str,
        incarnation: Option<&str>,
        expected_mission: &str,
        mission: &MissionSpec,
        idempotency_key: &str,
    ) -> Result<MissionOutputView, St3Error> {
        let subject = normalize_step_run(subject);
        let actor = normalize_actor(actor, "agent");
        let now = now_ms();
        self.connection
            .batched(|transaction| -> Result<MissionOutputView, St3Error> {
                if let Some(response) = smallclaims::store::idempotency::cached_response(transaction, idempotency_key)?
                {
                    return serde_json::from_str(&response).map_err(internal);
                }
                let current = transaction
                    .query_row(
                        "SELECT subject, run_id, step_path, definition_hash, status, attempt, assignee, available_to, agentless, title, goals, worker_reported,
                                lease_owner, lease_incarnation, lease_expires_at_unix_ms, blocked_reason, not_before_unix_ms, created_at_unix_ms, updated_at_unix_ms, readiness_epoch, constraints
                         FROM step_runs WHERE subject=?1",
                        [&subject],
                        step_run_from_row,
                    )
                    .optional()
                    .map_err(internal)?
                    .ok_or_else(|| {
                        St3Error::new(
                            "missing-step-run",
                            format!("step run `{subject}` does not exist"),
                        )
                    })?;
                if !step_generation_is_current(transaction, &current).map_err(internal)? {
                    return Err(St3Error::new(
                        "stale-run-generation",
                        format!("step run `{subject}` belongs to a superseded generation"),
                    ));
                }
                if !mission_output_authority(transaction, &current, &actor, incarnation, now)
                    .map_err(internal)?
                {
                    return Err(St3Error::new(
                        "work-not-claimed",
                        format!(
                            "`{actor}` does not hold an active lease for `{subject}` or its nested work"
                        ),
                    ));
                }
                if mission.id != expected_mission || mission.state != MissionState::Ready {
                    return Err(St3Error::new(
                        "wrong-mission-output",
                        format!("step run `{subject}` must publish ready mission `{expected_mission}`"),
                    ));
                }
                let stored = transaction
                    .query_row(
                        "SELECT state FROM mission_revisions WHERE mission_id=?1 AND revision=?2",
                        params![mission.id, mission.revision],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()
                    .map_err(internal)?;
                if stored.as_deref() != Some("ready") {
                    return Err(St3Error::new(
                        "unpublished-mission-output",
                        "the exact ready mission revision is not published",
                    ));
                }
                let fields: BTreeMap<String, Value> = BTreeMap::from([
                    (
                        "mission".into(),
                        Value::String(format!("mission/{}", mission.id)),
                    ),
                    ("revision".into(), Value::String(mission.revision.clone())),
                    (
                        "step_definition".into(),
                        Value::String(current.definition_hash.clone()),
                    ),
                    ("attempt".into(), Value::from(current.attempt)),
                ]);
                let body = json!({"fields": fields});
                let claim = append_receipt_claim_tx(
                    transaction,
                    &self.origin,
                    &subject,
                    "mission.produced",
                    Some(&actor),
                    &body,
                    &[],
                    None,
                    idempotency_key,
                )
                .map_err(internal)?;
                let output = MissionOutputView {
                    step: subject,
                    mission: format!("mission/{}", mission.id),
                    revision: mission.revision.clone(),
                    claim_id: claim.id,
                };
                transaction
                    .execute(
                        "INSERT INTO idempotency(operation_id, response) VALUES (?1, ?2)",
                        params![
                            opaque_cache_key(idempotency_key),
                            serde_json::to_string(&output).map_err(internal)?
                        ],
                    )
                    .map_err(internal)?;
                Ok(output)
            })
            .map_err(|error| St3Error::new("internal", error))?
    }

    pub fn mission_output_authorized(
        &self,
        subject: &str,
        actor: &str,
        incarnation: Option<&str>,
    ) -> Result<bool> {
        let subject = normalize_step_run(subject);
        let actor = normalize_actor(actor, "agent");
        let connection = self.readers.get();
        let current = connection
            .query_row(
                "SELECT subject, run_id, step_path, definition_hash, status, attempt, assignee, available_to, agentless, title, goals, worker_reported,
                        lease_owner, lease_incarnation, lease_expires_at_unix_ms, blocked_reason, not_before_unix_ms, created_at_unix_ms, updated_at_unix_ms, readiness_epoch, constraints
                 FROM step_runs WHERE subject=?1",
                [&subject],
                step_run_from_row,
            )
            .optional()
            .map_err(internal)?;
        current
            .as_ref()
            .map(|current| {
                Ok(step_generation_is_current(&connection, current)?
                    && mission_output_authority(
                        &connection,
                        current,
                        &actor,
                        incarnation,
                        now_ms(),
                    )?)
            })
            .transpose()
            .map(|authorized| authorized.unwrap_or(false))
    }

    pub fn mission_output(
        &self,
        subject: &str,
        attempt: u32,
        definition_hash: &str,
    ) -> Result<Option<MissionOutputView>> {
        let subject = normalize_step_run(subject);
        let Some(claim) = self.latest_claim(&subject, Some("mission.produced"))? else {
            return Ok(None);
        };
        let fields = claim.body.get("fields").unwrap_or(&claim.body);
        if fields.get("attempt").and_then(Value::as_u64) != Some(u64::from(attempt))
            || fields.get("step_definition").and_then(Value::as_str) != Some(definition_hash)
        {
            return Ok(None);
        }
        let Some(mission) = fields.get("mission").and_then(Value::as_str) else {
            return Ok(None);
        };
        let Some(revision) = fields.get("revision").and_then(Value::as_str) else {
            return Ok(None);
        };
        Ok(Some(MissionOutputView {
            step: subject,
            mission: mission.to_owned(),
            revision: revision.to_owned(),
            claim_id: claim.id,
        }))
    }

    pub fn mission_run(&self, run: &str) -> Result<Option<MissionRunView>> {
        let run = run.strip_prefix("mission-run/").unwrap_or(run);
        smallclaims::touched::note_read(|| format!("mission-run/{run}"));
        let connection = self.readers.get();
        let view = mission_run_view_tx(&connection, run).optional()?;
        if let Some(view) = &view {
            note_run_view_reads(view);
        }
        Ok(view)
    }

    /// A run with each step's effective state, and with `summaries` its latest progress and
    /// completion summaries, as `mission_run` shows them, without reading each step's timing,
    /// wake and definition history. Its loops and outcome are left empty.
    pub fn mission_run_steps(&self, run: &str, summaries: bool) -> Result<Option<MissionRunView>> {
        let run = run.strip_prefix("mission-run/").unwrap_or(run);
        smallclaims::touched::note_read(|| format!("mission-run/{run}"));
        let connection = self.readers.get();
        let view = mission_run_steps_view_tx(&connection, run, summaries).optional()?;
        if let Some(view) = &view {
            note_run_view_reads(view);
        }
        Ok(view)
    }

    /// `run`, a header with its current generation's steps as `mission_run_summaries_for_missions`
    /// reads them, with each step's effective state and, with `summaries`, its latest progress
    /// and completion summaries: `mission_run_steps` for a run whose steps are already read.
    pub fn with_step_states(
        &self,
        mut run: MissionRunView,
        summaries: bool,
    ) -> Result<MissionRunView> {
        let connection = self.readers.get();
        apply_step_states_tx(&connection, &mut run.steps, summaries)?;
        run.scheduler_fault = first_readiness_fault_tx(&connection, &run.subject)?;
        Ok(run)
    }

    /// Read one run's status without hydrating its step history.
    pub fn mission_run_status(&self, run: &str) -> Result<Option<String>> {
        let run = run.strip_prefix("mission-run/").unwrap_or(run);
        smallclaims::touched::note_read(|| format!("mission-run/{run}"));
        let connection = self.readers.get();
        Ok(connection
            .query_row(
                "SELECT status FROM mission_runs WHERE id=?1",
                [run],
                |row| row.get::<_, String>(0),
            )
            .optional()?)
    }

    /// Resolve the stable mission owner without hydrating the run's step history.
    pub fn mission_for_run(&self, run: &str) -> Result<Option<String>> {
        let run = run.strip_prefix("mission-run/").unwrap_or(run);
        let connection = self.readers.get();
        Ok(connection
            .query_row(
                "SELECT mission_id FROM mission_runs WHERE id=?1",
                [run],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .map(|id| format!("mission/{id}")))
    }

    pub fn run_generations(&self, run: &str) -> Result<Vec<RunGenerationView>> {
        let run = run.strip_prefix("mission-run/").unwrap_or(run);
        let connection = self.readers.get();
        let mut statement = connection.prepare(
            "SELECT id FROM run_generations WHERE run_id=?1 ORDER BY created_at_unix_ms, id",
        )?;
        let ids = statement
            .query_map([run], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        ids.into_iter()
            .map(|id| run_generation_view_tx(&connection, &id).map_err(Into::into))
            .collect()
    }

    pub fn run_generation(&self, generation: &str) -> Result<Option<RunGenerationView>> {
        let generation = generation_id_from_subject(generation);
        let connection = self.readers.get();
        Ok(run_generation_view_tx(&connection, generation).optional()?)
    }

    pub fn descendant_run_generations(&self, generation: &str) -> Result<Vec<String>> {
        let generation = generation_id_from_subject(generation);
        let connection = self.readers.get();
        descendant_mission_run_ids_tx(&connection, generation)?
            .into_iter()
            .map(|run| {
                connection
                    .query_row(
                        "SELECT current_generation_id FROM mission_runs WHERE id=?1",
                        [run],
                        |row| row.get::<_, String>(0),
                    )
                    .map(|id| format!("run-generation/{id}"))
                    .map_err(Into::into)
            })
            .collect()
    }

    pub fn revision_proposal_for_run(&self, run: &str) -> Result<Option<RevisionProposalView>> {
        let run = run.strip_prefix("mission-run/").unwrap_or(run);
        let connection = self.readers.get();
        let id = connection
            .query_row(
                &canonical_sql(
                    "SELECT p.id FROM revision_proposals p
                 JOIN mission_runs r ON r.id=p.run_id
                     JOIN claims created ON created.subject='revision-proposal/' || p.id
                       AND created.kind='revision-proposal.created'
                 WHERE p.run_id=?1 AND p.source_generation_id=r.current_generation_id
                   AND p.status IN ('pending-approval','draining')
                 ORDER BY CANONICAL_DESC(created) LIMIT 1",
                ),
                [run],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        id.map(|id| revision_proposal_view_tx(&connection, &id))
            .transpose()
            .map_err(Into::into)
    }

    pub fn revision_proposal(&self, proposal: &str) -> Result<Option<RevisionProposalView>> {
        let proposal = proposal
            .strip_prefix("revision-proposal/")
            .unwrap_or(proposal);
        let connection = self.readers.get();
        Ok(revision_proposal_view_tx(&connection, proposal).optional()?)
    }

    pub fn create_revision_proposal(
        &self,
        run: &str,
        mission: &MissionSpec,
        actor: &str,
        reason: &str,
        idempotency_key: &str,
    ) -> Result<RevisionProposalView, St3Error> {
        if let Some(response) = self
            .cached_idempotency_response(idempotency_key)
            .map_err(smallclaims::error::typed)?
        {
            return Ok(response);
        }
        let run_id = run.strip_prefix("mission-run/").unwrap_or(run);
        let current = self.mission_run(run_id).map_err(internal)?.ok_or_else(|| {
            St3Error::new(
                "missing-mission-run",
                format!("mission run `{run}` does not exist"),
            )
        })?;
        let reopening = is_failed_terminal(&current.status, &current.phase);
        if reopening {
            failed_run_reopen_blocker(&current).map_err(|reason| not_reopenable(run, &reason))?;
        } else if !matches!(current.status.as_str(), "running" | "standing" | "blocked")
            || current.phase != "normal"
        {
            return Err(St3Error::new(
                "mission-run-not-revisable",
                format!(
                    "mission run `{run}` is {} in its {} phase",
                    current.status, current.phase
                ),
            ));
        }
        let mission_id = current
            .mission
            .strip_prefix("mission/")
            .unwrap_or(&current.mission);
        let old = self
            .mission_spec(mission_id, Some(&current.revision))
            .map_err(internal)?
            .ok_or_else(|| {
                St3Error::new(
                    "missing-mission-revision",
                    "the current mission revision is unavailable",
                )
            })?;
        if mission.id != mission_id || mission.state != MissionState::Ready {
            return Err(St3Error::new(
                "wrong-mission-revision",
                format!("the proposal does not contain ready mission `{mission_id}`"),
            ));
        }
        if old.inputs != mission.inputs {
            return Err(St3Error::new(
                "run-input-mutation",
                "a run revision cannot change its input declarations",
            ));
        }
        let actor = normalize_actor(actor, "agent");
        let old = crate::mission::run_mission(old, current.after.as_deref())?;
        let mission = &crate::mission::run_mission(mission.clone(), current.after.as_deref())?;
        let (compatible, reviewers) = analyze_mission_revision(&old, mission, &current.requester)?;
        let compatible = carried_revision_step_paths(&old, mission, &current.steps, compatible);
        let cutover = old.revision_cutover.clone();
        let status = if reviewers.is_empty() {
            // A failed run has no active work to drain.
            if reopening || matches!(cutover, RevisionCutover::RestartActive) {
                return Err(St3Error::new(
                    "revision-does-not-need-proposal",
                    "this revision can cut over immediately",
                ));
            }
            "draining"
        } else {
            "pending-approval"
        };
        let proposal_id = hex::encode(Sha256::digest(
            format!("{}:{}:revision-proposal", self.origin, idempotency_key).as_bytes(),
        ))[..32]
            .to_owned();
        let preview_hash = hex::encode(Sha256::digest(
            serde_json::to_vec(&json!({
                "source_generation": current.generation,
                "candidate_revision": mission.revision,
                "compatible_steps": compatible,
                "reviewers": reviewers,
                "cutover": cutover,
            }))
            .map_err(internal)?,
        ));
        let now = now_ms();
        let mut connection = self.connection.write();
        if let Some(response) =
            smallclaims::store::idempotency::cached_response(&connection, idempotency_key)?
        {
            return serde_json::from_str(&response).map_err(internal);
        }
        if connection
            .query_row(
                "SELECT 1 FROM revision_proposals WHERE run_id=?1 AND status IN ('pending-approval','draining')",
                [run_id],
                |_| Ok(()),
            )
            .optional()
            .map_err(internal)?
            .is_some()
        {
            return Err(St3Error::new(
                "revision-proposal-already-pending",
                "the mission run already has one pending revision proposal",
            ));
        }
        let transaction = connection.transaction().map_err(internal)?;
        transaction
            .execute(
                "INSERT INTO revision_proposals(id, run_id, source_generation_id, candidate_revision, actor, reason, status, cutover, compatible_steps, reviewers, approvals, preview_hash, created_at_unix_ms, updated_at_unix_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, '[]', ?11, ?12, ?12)",
                params![
                    proposal_id,
                    run_id,
                    generation_id_from_subject(&current.generation),
                    mission.revision,
                    actor,
                    reason,
                    status,
                    revision_cutover_name(&cutover),
                    serde_json::to_string(&compatible).map_err(internal)?,
                    serde_json::to_string(&reviewers).map_err(internal)?,
                    preview_hash,
                    now.to_string(),
                ],
            )
            .map_err(internal)?;
        if status == "draining" {
            transaction
                .execute(
                    "UPDATE mission_runs SET phase='revision-draining', updated_at_unix_ms=?2 WHERE id=?1",
                    params![run_id, now.to_string()],
                )
                .map_err(internal)?;
        }
        let subject = format!("revision-proposal/{proposal_id}");
        let record = append_receipt_claim_tx(
            &transaction,
            &self.origin,
            &subject,
            "revision-proposal.created",
            Some(&actor),
            &json!({"fields": {
                "run": current.subject,
                "source_generation": current.generation,
                "candidate_revision": mission.revision,
                "reason": reason,
                "status": status,
                "cutover": revision_cutover_name(&cutover),
                "compatible_steps": compatible,
                "reviewers": reviewers,
                "preview_hash": preview_hash,
            }}),
            &[],
            None,
            idempotency_key,
        )
        .map_err(internal)?;
        if status == "draining" {
            transaction
                .execute(
                    "UPDATE mission_runs SET updated_at_unix_ms=?2 WHERE id=?1",
                    params![run_id, record.accepted_at_unix_ms.to_string()],
                )
                .map_err(internal)?;
        }

        let view = revision_proposal_view_tx(&transaction, &proposal_id).map_err(internal)?;
        transaction
            .execute(
                "INSERT INTO idempotency(operation_id, response) VALUES (?1, ?2)",
                params![
                    opaque_cache_key(idempotency_key),
                    serde_json::to_string(&view).map_err(internal)?
                ],
            )
            .map_err(internal)?;
        transaction.commit().map_err(internal)?;
        Ok(view)
    }

    pub fn approve_revision_proposal(
        &self,
        proposal: &str,
        actor: &str,
        preview_hash: &str,
        idempotency_key: &str,
    ) -> Result<RevisionSubmissionView, St3Error> {
        let proposal_id = proposal
            .strip_prefix("revision-proposal/")
            .unwrap_or(proposal);
        let actor = normalize_actor(actor, "person");
        let mut connection = self.connection.write();
        if let Some(response) =
            smallclaims::store::idempotency::cached_response(&connection, idempotency_key)?
        {
            return serde_json::from_str(&response).map_err(internal);
        }
        let transaction = connection.transaction().map_err(internal)?;
        let mut proposal =
            revision_proposal_view_tx(&transaction, proposal_id).map_err(internal)?;
        if proposal.status == "applied"
            && proposal.preview_hash.as_deref() == Some(preview_hash)
            && proposal.approvals.contains(&actor)
        {
            let run = mission_run_view_tx(
                &transaction,
                proposal
                    .run
                    .strip_prefix("mission-run/")
                    .unwrap_or(&proposal.run),
            )
            .map_err(internal)?;
            let response = RevisionSubmissionView {
                provenance: crate::provenance::read(
                    &transaction,
                    &run.mission,
                    &proposal.candidate_revision,
                )
                .map_err(internal)?,
                status: "applied".into(),
                mission_run: run,
                proposal: Some(proposal),
            };
            transaction
                .execute(
                    "INSERT INTO idempotency(operation_id, response) VALUES (?1, ?2)",
                    params![
                        opaque_cache_key(idempotency_key),
                        serde_json::to_string(&response).map_err(internal)?
                    ],
                )
                .map_err(internal)?;
            mark_replay_safe_receipt_tx(&transaction, idempotency_key).map_err(internal)?;
            transaction.commit().map_err(internal)?;
            return Ok(response);
        }
        if proposal.status != "pending-approval" {
            return Err(St3Error::new(
                "revision-proposal-not-reviewable",
                format!("revision proposal `{proposal_id}` is {}", proposal.status),
            ));
        }
        if proposal.preview_hash.as_deref() != Some(preview_hash) {
            return Err(St3Error::new(
                "stale-revision-preview",
                "the approval does not name the current revision preview",
            ));
        }
        if !proposal.reviewers.contains(&actor) {
            return Err(St3Error::new(
                "revision-approval-not-authorized",
                format!("`{actor}` is not a reviewer for `{proposal_id}`"),
            ));
        }
        if !proposal.approvals.contains(&actor) {
            proposal.approvals.push(actor.clone());
            proposal.approvals.sort();
        }
        let all_approved = proposal
            .reviewers
            .iter()
            .all(|reviewer| proposal.approvals.contains(reviewer));
        // A failed run has no active work to drain, so its approved revision cuts over now.
        let (run_status, run_phase) = transaction
            .query_row(
                "SELECT status, phase FROM mission_runs WHERE id=?1",
                [proposal
                    .run
                    .strip_prefix("mission-run/")
                    .unwrap_or(&proposal.run)],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .map_err(internal)?;
        let reopening = is_failed_terminal(&run_status, &run_phase);
        let status = if all_approved
            && !reopening
            && matches!(proposal.cutover, RevisionCutover::WhenIdle)
        {
            "draining"
        } else {
            "pending-approval"
        };
        let now = now_ms();
        transaction
            .execute(
                "UPDATE revision_proposals SET approvals=?2, status=?3, updated_at_unix_ms=?4 WHERE id=?1",
                params![
                    proposal_id,
                    serde_json::to_string(&proposal.approvals).map_err(internal)?,
                    status,
                    now.to_string(),
                ],
            )
            .map_err(internal)?;
        if status == "draining" {
            transaction
                .execute(
                    "UPDATE mission_runs SET phase='revision-draining', updated_at_unix_ms=?2 WHERE id=?1",
                    params![proposal.run.strip_prefix("mission-run/").unwrap_or(&proposal.run), now.to_string()],
                )
                .map_err(internal)?;
        }
        let record = append_receipt_claim_tx(
            &transaction,
            &self.origin,
            &proposal.subject,
            "revision-proposal.approved",
            Some(&actor),
            &json!({"fields": {"reviewer": actor, "preview_hash": preview_hash, "all_approved": all_approved}}),
            &[],
            None,
            idempotency_key,
        )
        .map_err(internal)?;
        if status == "draining" {
            transaction
                .execute(
                    "UPDATE mission_runs SET updated_at_unix_ms=?2 WHERE id=?1",
                    params![
                        proposal
                            .run
                            .strip_prefix("mission-run/")
                            .unwrap_or(&proposal.run),
                        record.accepted_at_unix_ms.to_string()
                    ],
                )
                .map_err(internal)?;
        }

        mark_replay_safe_receipt_tx(&transaction, idempotency_key).map_err(internal)?;
        transaction.commit().map_err(internal)?;
        drop(connection);

        if all_approved && (reopening || matches!(proposal.cutover, RevisionCutover::RestartActive))
        {
            let run = self.finish_proposal_cutover(proposal_id, &actor, idempotency_key)?;
            let applied = self
                .revision_proposal(proposal_id)
                .map_err(internal)?
                .expect("the applied proposal exists");
            let response = RevisionSubmissionView {
                provenance: self
                    .mission_provenance(&run.mission, &applied.candidate_revision)
                    .map_err(internal)?,
                status: "applied".into(),
                mission_run: run,
                proposal: Some(applied),
            };
            let connection = self.connection.write();
            connection
                .execute(
                    "INSERT INTO idempotency(operation_id, response) VALUES (?1, ?2)",
                    params![
                        opaque_cache_key(idempotency_key),
                        serde_json::to_string(&response).map_err(internal)?
                    ],
                )
                .map_err(internal)?;
            return Ok(response);
        }

        let proposal = self
            .revision_proposal(proposal_id)
            .map_err(internal)?
            .expect("the proposal exists");
        let run = self
            .mission_run(&proposal.run)
            .map_err(internal)?
            .expect("the proposal mission run exists");
        let response = RevisionSubmissionView {
            provenance: self
                .mission_provenance(&run.mission, &proposal.candidate_revision)
                .map_err(internal)?,
            status: proposal.status.clone(),
            mission_run: run,
            proposal: Some(proposal),
        };
        let connection = self.connection.write();
        connection
            .execute(
                "INSERT INTO idempotency(operation_id, response) VALUES (?1, ?2)",
                params![
                    opaque_cache_key(idempotency_key),
                    serde_json::to_string(&response).map_err(internal)?
                ],
            )
            .map_err(internal)?;
        Ok(response)
    }

    pub fn cancel_revision_proposal(
        &self,
        proposal: &str,
        actor: &str,
        reason: Option<&str>,
        idempotency_key: &str,
    ) -> Result<RevisionProposalView, St3Error> {
        let proposal_id = proposal
            .strip_prefix("revision-proposal/")
            .unwrap_or(proposal);
        let actor = normalize_actor(
            actor,
            if actor.starts_with("person/") {
                "person"
            } else {
                "agent"
            },
        );
        let mut connection = self.connection.write();
        if let Some(response) =
            smallclaims::store::idempotency::cached_response(&connection, idempotency_key)?
        {
            return serde_json::from_str(&response).map_err(internal);
        }
        let transaction = connection.transaction().map_err(internal)?;
        let current = revision_proposal_view_tx(&transaction, proposal_id).map_err(internal)?;
        let run_id = current
            .run
            .strip_prefix("mission-run/")
            .unwrap_or(&current.run);
        let requester: String = transaction
            .query_row(
                "SELECT requester FROM mission_runs WHERE id=?1",
                [run_id],
                |row| row.get(0),
            )
            .map_err(internal)?;
        if actor != current.actor && actor != requester && !current.reviewers.contains(&actor) {
            return Err(St3Error::new(
                "revision-cancel-not-authorized",
                format!("`{actor}` cannot cancel `{proposal_id}`"),
            ));
        }
        if !matches!(current.status.as_str(), "pending-approval" | "draining") {
            return Err(St3Error::new(
                "revision-proposal-not-cancellable",
                format!("revision proposal `{proposal_id}` is {}", current.status),
            ));
        }
        let now = now_ms();
        transaction
            .execute(
                "UPDATE revision_proposals SET status='cancelled', updated_at_unix_ms=?2 WHERE id=?1",
                params![proposal_id, now.to_string()],
            )
            .map_err(internal)?;
        let phase_changed = transaction
            .execute(
                "UPDATE mission_runs SET phase='normal', updated_at_unix_ms=?2 WHERE id=?1 AND phase='revision-draining'",
                params![run_id, now.to_string()],
            )
            .map_err(internal)?;
        let record = append_receipt_claim_tx(
            &transaction,
            &self.origin,
            &current.subject,
            "revision-proposal.cancelled",
            Some(&actor),
            &json!({"fields": {"status": "cancelled", "reason": reason}}),
            &[],
            None,
            idempotency_key,
        )
        .map_err(internal)?;
        if phase_changed > 0 {
            transaction
                .execute(
                    "UPDATE mission_runs SET updated_at_unix_ms=?2 WHERE id=?1",
                    params![run_id, record.accepted_at_unix_ms.to_string()],
                )
                .map_err(internal)?;
        }

        let view = revision_proposal_view_tx(&transaction, proposal_id).map_err(internal)?;
        transaction
            .execute(
                "INSERT INTO idempotency(operation_id, response) VALUES (?1, ?2)",
                params![
                    opaque_cache_key(idempotency_key),
                    serde_json::to_string(&view).map_err(internal)?
                ],
            )
            .map_err(internal)?;
        transaction.commit().map_err(internal)?;
        Ok(view)
    }

    pub fn finish_proposal_cutover(
        &self,
        proposal_id: &str,
        actor: &str,
        idempotency_key: &str,
    ) -> Result<MissionRunView, St3Error> {
        let proposal = self
            .revision_proposal(proposal_id)
            .map_err(internal)?
            .ok_or_else(|| {
                St3Error::new(
                    "missing-revision-proposal",
                    format!("revision proposal `{proposal_id}` does not exist"),
                )
            })?;
        let run_id = proposal
            .run
            .strip_prefix("mission-run/")
            .unwrap_or(&proposal.run);
        let current = self.mission_run(run_id).map_err(internal)?.ok_or_else(|| {
            St3Error::new(
                "missing-mission-run",
                format!("mission run `{run_id}` does not exist"),
            )
        })?;
        if current.generation != proposal.source_generation {
            return Err(St3Error::new(
                "stale-revision-proposal",
                "the revision proposal does not target the current generation",
            ));
        }
        let mission_id = current
            .mission
            .strip_prefix("mission/")
            .unwrap_or(&current.mission);
        let mission = self
            .mission_spec(mission_id, Some(&proposal.candidate_revision))
            .map_err(internal)?
            .ok_or_else(|| {
                St3Error::new(
                    "missing-mission-revision",
                    "the proposed mission revision is unavailable",
                )
            })?;
        self.adopt_approved_mission_revision(
            run_id,
            &mission,
            actor,
            &proposal.reason,
            &format!("{idempotency_key}:cutover"),
            &proposal.source_generation,
            &proposal,
        )
    }

    pub fn mission_run_for_parent_step(&self, step: &str) -> Result<Option<MissionRunView>> {
        let step = normalize_step_run(step);
        smallclaims::touched::note_read(|| format!("children-of-step:{step}"));
        let connection = self.connection.write();
        let run_id = connection
            .query_row(
                "SELECT id FROM mission_runs WHERE parent_step_run=?1 ORDER BY created_at_unix_ms DESC LIMIT 1",
                [step],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        let view = run_id
            .map(|run_id| mission_run_view_tx(&connection, &run_id))
            .transpose()?;
        if let Some(view) = &view {
            note_run_view_reads(view);
        }
        Ok(view)
    }

    pub fn step_has_active_child_runs(&self, step: &str) -> Result<bool> {
        let step = normalize_step_run(step);
        smallclaims::touched::note_read(|| format!("children-of-step:{step}"));
        let connection = self.readers.get();
        let mut statement = connection.prepare_cached(
            "SELECT 'mission-run/' || id, status FROM mission_runs WHERE parent_step_run=?1",
        )?;
        let rows = statement.query_map([step], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (subject, status) = row?;
            smallclaims::touched::note_read(|| subject);
            if !matches!(status.as_str(), "completed" | "failed" | "cancelled") {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Linked runs with their effective current steps, without presentation or work history.
    /// Read the parent link even when empty, so a new round invalidates its parent's evaluation.
    pub fn mission_run_children_for_step(&self, step: &str) -> Result<Vec<MissionRunView>> {
        let step = normalize_step_run(step);
        smallclaims::touched::note_read(|| format!("children-of-step:{step}"));
        let connection = self.readers.get();
        let ids = connection
            .prepare_cached("SELECT id FROM mission_runs WHERE parent_step_run=?1")?
            .query_map([step], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ids.into_iter()
            .map(|id| {
                let view = mission_run_view_for_projection_tx(&connection, &id)?;
                note_run_view_reads(&view);
                Ok(view)
            })
            .collect()
    }

    pub fn adopt_mission_revision(
        &self,
        run: &str,
        mission: &MissionSpec,
        actor: &str,
        reason: &str,
        idempotency_key: &str,
    ) -> Result<MissionRunView, St3Error> {
        self.adopt_mission_revision_inner(
            run,
            mission,
            actor,
            reason,
            idempotency_key,
            None,
            false,
            None,
            false,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn adopt_approved_mission_revision(
        &self,
        run: &str,
        mission: &MissionSpec,
        actor: &str,
        reason: &str,
        idempotency_key: &str,
        source_generation: &str,
        proposal: &RevisionProposalView,
    ) -> Result<MissionRunView, St3Error> {
        self.adopt_mission_revision_inner(
            run,
            mission,
            actor,
            reason,
            idempotency_key,
            Some(source_generation),
            true,
            Some(proposal),
            false,
        )
    }

    /// Retry one failed step. While its run is active, the step retries in place. A failed run
    /// reopens in a successor generation of its current revision when this step is the only
    /// reason it failed.
    pub fn retry_failed_step(
        &self,
        subject: &str,
        actor: &str,
        reason: &str,
        idempotency_key: &str,
    ) -> Result<MissionRunView, St3Error> {
        // Hold the writer from the dedupe lookup through the transition and receipt commit.
        let mut connection = self.connection.write();
        if let Some(response) =
            smallclaims::store::idempotency::cached_response(&connection, idempotency_key)?
        {
            return serde_json::from_str(&response).map_err(internal);
        }
        let subject = normalize_step_run(subject);
        let step = self.step_run(&subject).map_err(internal)?.ok_or_else(|| {
            St3Error::new(
                "missing-step-run",
                format!("step run `{subject}` does not exist"),
            )
        })?;
        let current = self
            .mission_run(&step.run)
            .map_err(internal)?
            .ok_or_else(|| {
                St3Error::new(
                    "missing-mission-run",
                    format!("mission run `{}` does not exist", step.run),
                )
            })?;
        if generation_id_from_subject(&step.generation)
            != generation_id_from_subject(&current.generation)
        {
            return Err(St3Error::new(
                "stale-run-generation",
                format!("step run `{subject}` belongs to a superseded generation"),
            ));
        }
        if step.status != "failed" {
            return Err(St3Error::new(
                "work-not-failed",
                format!("step run `{subject}` is `{}`, not failed", step.status),
            ));
        }
        let mission_id = current
            .mission
            .strip_prefix("mission/")
            .unwrap_or(&current.mission);
        let mission = self
            .mission_spec(mission_id, Some(&current.revision))
            .map_err(internal)?
            .ok_or_else(|| {
                St3Error::new(
                    "missing-mission-revision",
                    "the current mission revision is unavailable",
                )
            })?;
        if is_failed_terminal(&current.status, &current.phase) {
            let failed = failed_run_reopen_blocker(&current)
                .map_err(|reason| not_reopenable(&current.subject, &reason))?;
            if let Some(other) = failed.iter().find(|path| **path != step.step) {
                return Err(not_reopenable(
                    &current.subject,
                    &format!(
                        "step `{other}` also failed; revise the run to restart every failed step"
                    ),
                ));
            }
            drop(connection);
            return self.adopt_mission_revision_inner(
                &current.id,
                &mission,
                actor,
                reason,
                idempotency_key,
                Some(&current.generation),
                false,
                None,
                true,
            );
        }
        let finally = flatten_mission_step_specs(&mission)
            .into_iter()
            .any(|spec| spec.path == step.step && spec.finally);
        let phase_runs_step = if finally {
            matches!(current.phase.as_str(), "final" | "final-cancelled")
        } else {
            current.phase == "normal"
        };
        if !matches!(current.status.as_str(), "running" | "standing" | "blocked")
            || !phase_runs_step
        {
            return Err(St3Error::new(
                "mission-run-not-retryable",
                format!(
                    "mission run `{}` is {} in its {} phase; retry the step after the run fails",
                    current.subject, current.status, current.phase
                ),
            ));
        }
        let transaction = connection.transaction().map_err(internal)?;
        if !retry_step_tx(
            &transaction,
            &self.origin,
            &subject,
            Some(actor),
            reason,
            0,
            Some(idempotency_key),
        )
        .map_err(internal)?
        {
            return Err(St3Error::new(
                "work-not-retryable",
                format!("step run `{subject}` changed before the retry"),
            ));
        }
        let view = mission_run_view_tx(&transaction, &current.id).map_err(internal)?;
        transaction
            .execute(
                "INSERT INTO idempotency(operation_id,response) VALUES(?1,?2)",
                params![
                    opaque_cache_key(idempotency_key),
                    serde_json::to_string(&view).map_err(internal)?
                ],
            )
            .map_err(internal)?;
        transaction.commit().map_err(internal)?;
        Ok(view)
    }

    /// Set the outcome of a finished root run to `completed`, `failed` or `cancelled`. The run
    /// and its steps stay as they ended; the new outcome, who set it and why are its latest
    /// state claims, so every node and client shows the same outcome.
    pub fn set_mission_run_outcome(
        &self,
        run: &str,
        status: &str,
        actor: &str,
        reason: &str,
        idempotency_key: &str,
    ) -> Result<MissionRunView, St3Error> {
        if let Some(response) = self
            .cached_idempotency_response(idempotency_key)
            .map_err(smallclaims::error::typed)?
        {
            return Ok(response);
        }
        if !is_terminal_run_state(status) {
            return Err(St3Error::new(
                "invalid-run-outcome",
                format!("a run outcome is completed, failed or cancelled, not `{status}`"),
            ));
        }
        let reason = reason.trim();
        if reason.is_empty() {
            return Err(St3Error::new(
                "missing-outcome-reason",
                "setting a run outcome needs a reason",
            ));
        }
        let actor = normalize_actor(actor, "agent");
        let run_id = run.strip_prefix("mission-run/").unwrap_or(run);
        let mut connection = self.connection.write();
        let transaction = connection.transaction().map_err(internal)?;
        if let Some(response) = smallclaims::store::idempotency::cached_response(&transaction,idempotency_key)? {
            return serde_json::from_str(&response).map_err(internal);
        }
        let current = mission_run_header_tx(&transaction, run_id)
            .optional()
            .map_err(internal)?
            .ok_or_else(|| {
                St3Error::new(
                    "missing-mission-run",
                    format!("mission run `mission-run/{run_id}` does not exist"),
                )
            })?;
        if current.phase != "terminal" || !is_terminal_run_state(&current.status) {
            return Err(St3Error::new(
                "mission-run-not-finished",
                format!(
                    "mission run `{}` is {} in its {} phase; only a finished run takes an outcome, so cancel it with `st missions cancel` or let it finish",
                    current.subject, current.status, current.phase
                ),
            ));
        }
        if current.parent_step_run.is_some() {
            return Err(St3Error::new(
                "child-run-outcome",
                format!(
                    "mission run `{}` is a child run; its outcome belongs to its parent step",
                    current.subject
                ),
            ));
        }
        if current.mode == "eval" {
            return Err(St3Error::new(
                "eval-run-outcome",
                format!(
                    "mission run `{}` is an eval run and keeps its verdict",
                    current.subject
                ),
            ));
        }
        if current.status == status {
            return Err(St3Error::new(
                "mission-run-outcome-unchanged",
                format!("mission run `{}` is already {status}", current.subject),
            ));
        }
        let now = now_ms();
        let generation = generation_id_from_subject(&current.generation).to_owned();
        transaction
            .execute(
                "UPDATE mission_runs SET status=?2, updated_at_unix_ms=?3 WHERE id=?1",
                params![run_id, status, now.to_string()],
            )
            .map_err(internal)?;
        transaction
            .execute(
                "UPDATE run_generations SET status=?2, updated_at_unix_ms=?3 WHERE id=?1",
                params![generation, status, now.to_string()],
            )
            .map_err(internal)?;
        // A terminal state that follows a terminal state is an outcome someone set; the
        // reconciler never writes one.
        append_receipt_claim_tx(
            &transaction,
            &self.origin,
            &current.subject,
            "mission-run.state",
            Some(&actor),
            &json!({"fields": {
                "status": status,
                "phase": "terminal",
                "previous_phase": "terminal",
                "reason": reason,
            }}),
            &[],
            None,
            idempotency_key,
        )
        .map_err(internal)?;
        append_claim_tx(
            &transaction,
            &self.origin,
            &current.generation,
            "run-generation.state",
            Some(&actor),
            &json!({"fields": {"status": status, "phase": "terminal", "reason": reason}}),
            &[],
            None,
        )
        .map_err(internal)?;
        let view = mission_run_view_tx(&transaction, run_id).map_err(internal)?;
        transaction
            .execute(
                "INSERT INTO idempotency(operation_id, response) VALUES (?1, ?2)",
                params![
                    opaque_cache_key(idempotency_key),
                    serde_json::to_string(&view).map_err(internal)?
                ],
            )
            .map_err(internal)?;
        transaction.commit().map_err(internal)?;
        Ok(view)
    }

    /// Change who a running run reports to, or clear it, as `missions start --report-to` would
    /// have set it: the mission's own `stalled-after`, `report-stalls` and `report-completed`
    /// apply unless the request names a limit or stall policy. Only a person or the agent that
    /// requested the run may. It is a claim of its own on the run, so the creation claim keeps what the run started with and a
    /// repeated declaration of the run still compares with it. A request that changes nothing
    /// writes nothing.
    #[allow(clippy::too_many_arguments)]
    pub fn set_mission_run_report(
        &self,
        run: &str,
        actor: &str,
        report_to: Option<&str>,
        stalled_after_ms: Option<u64>,
        report_stalls: Option<bool>,
        report_completed: bool,
        idempotency_key: &str,
    ) -> Result<MissionRunReportView, St3Error> {
        if let Some(response) = self
            .cached_idempotency_response(idempotency_key)
            .map_err(smallclaims::error::typed)?
        {
            return Ok(response);
        }
        match report_to {
            None if stalled_after_ms.is_some() || report_stalls.is_some() || report_completed => {
                return Err(St3Error::new(
                    "report-without-recipient",
                    "a stall policy, limit or completion report needs a `report-to` agent",
                ));
            }
            None => {}
            Some(report_to) => crate::mission::validate_report_to(report_to)?,
        }
        if stalled_after_ms == Some(0) {
            return Err(St3Error::new(
                "invalid-stalled-after",
                "stalled-after must be greater than zero",
            ));
        }
        let actor = normalize_actor(actor, "agent");
        let run_id = run.strip_prefix("mission-run/").unwrap_or(run);
        let mut connection = self.connection.write();
        let transaction = connection.transaction().map_err(internal)?;
        if let Some(response) =
            smallclaims::store::idempotency::cached_response(&transaction, idempotency_key)?
        {
            return serde_json::from_str(&response).map_err(internal);
        }
        let current = mission_run_header_tx(&transaction, run_id)
            .optional()
            .map_err(internal)?
            .ok_or_else(|| {
                St3Error::new(
                    "missing-mission-run",
                    format!("mission run `mission-run/{run_id}` does not exist"),
                )
            })?;
        if current.phase == "terminal" {
            return Err(St3Error::new(
                "mission-run-not-running",
                format!(
                    "mission run `{}` has finished as {}; only a running run takes a reporter",
                    current.subject, current.status
                ),
            ));
        }
        if !actor.starts_with("person/") && actor != current.requester {
            return Err(St3Error::new(
                "run-report-authority-denied",
                format!(
                    "only a person or the run's requester `{}` may change who `{}` reports to",
                    current.requester, current.subject
                ),
            ));
        }
        let desired = match report_to {
            None => None,
            Some(report_to) => {
                let mission = mission_revision_spec_tx(
                    &transaction,
                    current
                        .mission
                        .strip_prefix("mission/")
                        .unwrap_or(&current.mission),
                    &current.revision,
                )?;
                Some((
                    report_to.to_owned(),
                    stalled_after_ms
                        .or(mission.as_ref().and_then(|mission| mission.stalled_after_ms))
                        .unwrap_or(crate::mission::DEFAULT_STALLED_AFTER_MS),
                    report_completed
                        || mission.as_ref().is_some_and(|mission| mission.report_completed),
                    report_stalls.unwrap_or_else(|| {
                        mission.as_ref().is_none_or(|mission| mission.report_stalls)
                    }),
                ))
            }
        };
        let changed = desired != current_run_report_tx(&transaction, &current.subject)?;
        if changed {
            let mut fields = serde_json::Map::new();
            if let Some((report_to, stalled_after_ms, completed, stalls)) = &desired {
                fields.insert("report_to".into(), json!(report_to));
                fields.insert("stalled_after_ms".into(), json!(stalled_after_ms));
                if !*stalls {
                    fields.insert("report_stalls".into(), json!(false));
                }
                if *completed {
                    fields.insert("report_completed".into(), json!(true));
                }
            }
            append_receipt_claim_tx(
                &transaction,
                &self.origin,
                &current.subject,
                RUN_REPORT_KIND,
                Some(&actor),
                &json!({ "fields": fields }),
                &[],
                None,
                idempotency_key,
            )
            .map_err(internal)?;
        }
        let (report_to, stalled_after_ms, report_completed, report_stalls) = match desired {
            Some((report_to, stalled_after_ms, completed, stalls)) => {
                (Some(report_to), Some(stalled_after_ms), completed, stalls)
            }
            None => (None, None, false, true),
        };
        let view = MissionRunReportView {
            run: current.subject,
            report_to,
            stalled_after_ms,
            report_stalls,
            report_completed,
            changed,
        };
        transaction
            .execute(
                "INSERT INTO idempotency(operation_id, response) VALUES (?1, ?2)",
                params![
                    opaque_cache_key(idempotency_key),
                    serde_json::to_string(&view).map_err(internal)?
                ],
            )
            .map_err(internal)?;
        transaction.commit().map_err(internal)?;
        Ok(view)
    }

    /// Retire a published mission: publish its current definition again as `retired`, so it
    /// leaves the mission lists and cannot start, while every revision and run stays in its
    /// history. The publication names who retired it. Publishing a ready revision brings the
    /// mission back.
    pub fn retire_mission(
        &self,
        mission: &str,
        actor: &str,
        idempotency_key: &str,
    ) -> Result<MissionSpec, St3Error> {
        if let Some(response) = self
            .cached_idempotency_response(idempotency_key)
            .map_err(smallclaims::error::typed)?
        {
            return Ok(response);
        }
        let mission_id = mission.strip_prefix("mission/").unwrap_or(mission);
        let actor = normalize_actor(actor, "agent");
        let current = self
            .mission_spec(mission_id, None)
            .map_err(internal)?
            .ok_or_else(|| {
                St3Error::new(
                    "missing-mission",
                    format!("mission `mission/{mission_id}` is not published"),
                )
            })?;
        if current.state == MissionState::Retired {
            return Err(St3Error::new(
                "mission-already-retired",
                format!("mission `mission/{mission_id}` is already retired"),
            ));
        }
        let retired = crate::mission::retired_mission(current)?;
        let mut connection = self.connection.write();
        let transaction = connection.transaction().map_err(internal)?;
        if let Some(response) = smallclaims::store::idempotency::cached_response(&transaction,idempotency_key)? {
            return serde_json::from_str(&response).map_err(internal);
        }
        owned_sets::refuse_unmanaged(&transaction, &format!("mission/{mission_id}"))?;
        let active = transaction
            .query_row(
                "SELECT id FROM mission_runs
                 WHERE mission_id=?1 AND status IN ('running','standing','blocked')
                 ORDER BY created_at_unix_ms, id LIMIT 1",
                [mission_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(internal)?;
        if let Some(run) = active {
            return Err(St3Error::new(
                "mission-has-active-run",
                format!(
                    "mission `mission/{mission_id}` has the active run `mission-run/{run}`; cancel it before retiring the mission"
                ),
            ));
        }
        let predecessors =
            mission_definition_token_tx(&transaction, mission_id).map_err(internal)?;
        let body = serde_json::to_value(&retired).map_err(internal)?;
        let claim = append_receipt_claim_tx(
            &transaction,
            &self.origin,
            &retired.subject,
            "mission.published",
            Some(&actor),
            &body,
            &predecessors,
            None,
            idempotency_key,
        )
        .map_err(internal)?;
        transaction
            .execute(
                "INSERT OR IGNORE INTO mission_revisions(mission_id, revision, state, body, claim_id, created_index) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![retired.id, retired.revision, mission_state_name(&retired.state), serde_json::to_string(&retired).map_err(internal)?, claim.id, claim.store_index],
            )
            .map_err(internal)?;
        transaction
            .execute(
                "INSERT INTO mission_definitions(mission_id, revision, state, claim_id) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(mission_id) DO UPDATE SET revision=excluded.revision, state=excluded.state, claim_id=excluded.claim_id",
                params![retired.id, retired.revision, mission_state_name(&retired.state), claim.id],
            )
            .map_err(internal)?;
        transaction
            .execute(
                "INSERT INTO idempotency(operation_id, response) VALUES (?1, ?2)",
                params![
                    opaque_cache_key(idempotency_key),
                    serde_json::to_string(&retired).map_err(internal)?
                ],
            )
            .map_err(internal)?;
        transaction.commit().map_err(internal)?;
        Ok(retired)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn adopt_mission_revision_inner(
        &self,
        run: &str,
        mission: &MissionSpec,
        actor: &str,
        reason: &str,
        idempotency_key: &str,
        expected_generation: Option<&str>,
        protected_approved: bool,
        proposal: Option<&RevisionProposalView>,
        retry: bool,
    ) -> Result<MissionRunView, St3Error> {
        if let Some(response) = self
            .cached_idempotency_response(idempotency_key)
            .map_err(smallclaims::error::typed)?
        {
            return Ok(response);
        }
        let run_id = run.strip_prefix("mission-run/").unwrap_or(run);
        let actor = normalize_actor(actor, "agent");
        let current = self.mission_run(run_id).map_err(internal)?.ok_or_else(|| {
            St3Error::new(
                "missing-mission-run",
                format!("mission run `{run}` does not exist"),
            )
        })?;
        if expected_generation.is_some_and(|expected| {
            generation_id_from_subject(expected) != generation_id_from_subject(&current.generation)
        }) {
            return Err(St3Error::new(
                "stale-revision-proposal",
                "the revision proposal does not target the current generation",
            ));
        }
        let reopening = is_failed_terminal(&current.status, &current.phase);
        let phase_allows_cutover = current.phase == "normal"
            || (proposal.is_some() && current.phase == "revision-draining");
        if reopening {
            failed_run_reopen_blocker(&current).map_err(|reason| not_reopenable(run, &reason))?;
        } else if retry
            || !matches!(current.status.as_str(), "running" | "standing" | "blocked")
            || !phase_allows_cutover
        {
            return Err(St3Error::new(
                "mission-run-not-revisable",
                format!(
                    "mission run `{run}` is {} in its {} phase",
                    current.status, current.phase
                ),
            ));
        }
        let mission_id = current
            .mission
            .strip_prefix("mission/")
            .unwrap_or(&current.mission);
        if mission.id != mission_id {
            return Err(St3Error::new(
                "wrong-mission-revision",
                format!(
                    "revision `{}` does not replace mission `{mission_id}`",
                    mission.id
                ),
            ));
        }
        if mission.state != MissionState::Ready {
            return Err(St3Error::new(
                "mission-revision-not-ready",
                "a running mission can adopt only a ready revision",
            ));
        }
        let old = self
            .mission_spec(mission_id, Some(&current.revision))
            .map_err(internal)?
            .ok_or_else(|| {
                St3Error::new(
                    "missing-mission-revision",
                    "the current mission revision is unavailable",
                )
            })?;
        if old.inputs != mission.inputs {
            return Err(St3Error::new(
                "run-input-mutation",
                "a run revision cannot change its input declarations",
            ));
        }
        let old = crate::mission::run_mission(old, current.after.as_deref())?;
        let mission = &crate::mission::run_mission(mission.clone(), current.after.as_deref())?;

        let variables = mission_run_variables(&current, &mission.revision);
        // A retry keeps the current revision, so it has no change to analyze or review.
        let (compatible, reviewers) = if protected_approved || retry {
            (compatible_step_paths(&old, mission), BTreeSet::new())
        } else {
            analyze_mission_revision(&old, mission, &current.requester)?
        };
        let mut compatible = carried_revision_step_paths(&old, mission, &current.steps, compatible);
        if reopening {
            retain_reopened_step_paths(mission, &current.steps, &mut compatible);
        }
        if !protected_approved
            && !reopening
            && matches!(old.revision_cutover, RevisionCutover::WhenIdle)
        {
            return Err(St3Error::new(
                "revision-needs-drained-cutover",
                "the current mission revision requires a drained cutover",
            ));
        }
        if !reviewers.is_empty() && !protected_approved {
            return Err(St3Error::new(
                "revision-needs-human-approval",
                format!(
                    "the revision needs approval from {}",
                    reviewers.into_iter().collect::<Vec<_>>().join(", ")
                ),
            ));
        }

        let mut new_steps = Vec::new();
        flatten_steps(mission, None, &[], &mut new_steps);

        let mut connection = self.connection.write();
        if let Some(response) =
            smallclaims::store::idempotency::cached_response(&connection, idempotency_key)?
        {
            return serde_json::from_str(&response).map_err(internal);
        }
        let transaction = connection.transaction().map_err(internal)?;
        if reopening {
            enforce_mission_run_capacity(&transaction, mission)?;
        }
        let now = now_ms();
        let predecessor_id = generation_id_from_subject(&current.generation).to_owned();
        let generation_id = hex::encode(Sha256::digest(
            format!("{}:{}:generation", self.origin, idempotency_key).as_bytes(),
        ))[..32]
            .to_owned();
        let generation_subject = format!("run-generation/{generation_id}");
        let predecessor_subject = current.generation.clone();
        let mut successor_variables = variables.clone();
        successor_variables.insert("ST_RUN_GENERATION".into(), generation_id.clone());
        transaction
            .execute(
                "INSERT INTO run_generations(id, run_id, revision, predecessor_id, status, actor, reason, created_at_unix_ms, updated_at_unix_ms)
                 VALUES (?1, ?2, ?3, ?4, 'running', ?5, ?6, ?7, ?7)",
                params![generation_id, run_id, mission.revision, predecessor_id, actor, reason, now.to_string()],
            )
            .map_err(internal)?;
        for (step, selector, constraints) in new_steps {
            let (assignee, available_to, agentless) =
                interpolate_selector(&selector, &successor_variables)?;
            let subject = format!("step-run/{generation_id}/{}", step.path);
            let mut step_variables = successor_variables.clone();
            step_variables.insert("ST_STEP".into(), step.path.clone());
            step_variables.insert("ST_STEP_RUN".into(), subject.clone());
            step_variables.insert("ST_ASSIGNEE".into(), assignee.clone().unwrap_or_default());
            step_variables.insert(
                "ST_PARENT_STEP_RUN".into(),
                crate::mission::parent_step_path(mission, &step.path)
                    .map(|path| format!("step-run/{generation_id}/{path}"))
                    .or_else(|| current.parent_step_run.clone())
                    .unwrap_or_default(),
            );
            let carried = compatible
                .contains(&step.path)
                .then(|| current.steps.iter().find(|old| old.step == step.path))
                .flatten();
            // Reopening a failed run starts each carried failed step at its next attempt.
            let retried = carried.filter(|old| reopening && old.status == "failed");
            let attempt = carried
                .map(|old| old.attempt + u32::from(retried.is_some()))
                .unwrap_or(1);
            step_variables.insert("ST_ATTEMPT".into(), attempt.to_string());
            let title = step
                .title
                .as_deref()
                .map(|value| crate::mission::interpolate(value, &step_variables))
                .transpose()?;
            let goals = interpolate_goals(&step.goals, &step_variables)?;
            let constraints = interpolate_goals(&constraints, &step_variables)?;
            let carried_state = carried.filter(|_| retried.is_none());
            let (status, worker_reported) = carried_state
                .map(carried_step_projection)
                .unwrap_or(("pending", false));
            let (lease_owner, lease_incarnation, lease_expires) =
                carried_step_lease(carried_state, status);
            let blocked_reason = carried_state.and_then(|old| old.blocked_reason.as_deref());
            let not_before = carried_state
                .and_then(|old| old.not_before_unix_ms)
                .map(|value| value.to_string());
            transaction
                .execute(
                    "INSERT INTO step_runs(subject, run_id, generation_id, step_path, definition_hash, status, attempt, assignee, available_to, agentless, title, goals, worker_reported, blocked_reason, not_before_unix_ms, readiness_epoch, created_at_unix_ms, updated_at_unix_ms, constraints, lease_owner, lease_incarnation, lease_expires_at_unix_ms)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?17, ?18, ?19, ?20, ?21)",
                    params![subject, run_id, generation_id, step.path, step.definition_hash, status, attempt, assignee, serde_json::to_string(&available_to).map_err(internal)?, agentless, title, goals, worker_reported, blocked_reason, not_before, carried.map(|old| old.readiness_epoch).unwrap_or(0), now.to_string(), constraints, lease_owner, lease_incarnation, lease_expires.map(|expiry| expiry.to_string())],
                )
                .map_err(internal)?;
            if let Some(old) = carried {
                let mut fields = json!({
                    "source_step_run": old.subject,
                    "source_generation": predecessor_subject,
                    "status": status,
                    "attempt": attempt,
                    "worker_reported": worker_reported,
                    "claimant": lease_owner,
                    "claim_incarnation": lease_incarnation,
                    "claim_expires_at_unix_ms": lease_expires
                });
                if let Some(claimant) = dropped_claimant(old, status) {
                    fields["claimant"] = Value::String(claimant.into());
                }
                append_claim_tx(
                    &transaction,
                    &self.origin,
                    &subject,
                    "step-run.carried",
                    Some(&actor),
                    &json!({"fields": fields}),
                    &[],
                    None,
                )
                .map_err(internal)?;
            }
        }
        revision_seats::carry_completed_agents_tx(
            &transaction,
            &self.origin,
            &current,
            mission,
            &compatible,
            &generation_subject,
            None,
        )?;
        cancel_descendant_mission_runs_tx(
            &transaction,
            &self.origin,
            &predecessor_id,
            &actor,
            "the parent run generation was superseded",
            now,
        )?;
        transaction
            .execute(
                "UPDATE run_generations SET status='superseded', updated_at_unix_ms=?2 WHERE id=?1",
                params![predecessor_id, now.to_string()],
            )
            .map_err(internal)?;
        transaction
            .execute(
                "UPDATE mission_runs SET current_generation_id=?2, status='running', phase='normal', updated_at_unix_ms=?3 WHERE id=?1",
                params![run_id, generation_id, now.to_string()],
            )
            .map_err(internal)?;
        append_receipt_claim_tx(
            &transaction,
            &self.origin,
            &predecessor_subject,
            "run-generation.superseded",
            Some(&actor),
            &json!({"fields": {"status": "superseded", "successor": generation_subject, "reason": reason}}),
            &[],
            None,
            idempotency_key,
        )
        .map_err(internal)?;
        terminalize_generation_steps_tx(
            &transaction,
            &self.origin,
            &predecessor_id,
            "the owning run generation was superseded",
            Some(&actor),
            None,
            now,
        )?;
        append_claim_tx(
            &transaction,
            &self.origin,
            &generation_subject,
            "run-generation.created",
            Some(&actor),
            &json!({"fields": {
                "run": current.subject,
                "revision": mission.revision,
                "predecessor": predecessor_subject,
                "status": "running",
                "reason": reason,
                "compatible_steps": compatible
            }}),
            &[],
            None,
        )
        .map_err(internal)?;
        if reopening {
            // The run's own state history says who reopened it, when, and why.
            append_claim_tx(
                &transaction,
                &self.origin,
                &current.subject,
                "mission-run.state",
                Some(&actor),
                &json!({"fields": {
                    "status": "running",
                    "phase": "normal",
                    "previous_phase": current.phase,
                    "reason": reason,
                }}),
                &[],
                None,
            )
            .map_err(internal)?;
        }
        if let Some(proposal) = proposal {
            let changed = transaction
                .execute(
                    "UPDATE revision_proposals
                     SET status='applied', successor_generation_id=?2, updated_at_unix_ms=?3
                     WHERE id=?1 AND source_generation_id=?4
                       AND status IN ('pending-approval','draining')",
                    params![proposal.id, generation_id, now.to_string(), predecessor_id],
                )
                .map_err(internal)?;
            if changed != 1 {
                return Err(St3Error::new(
                    "stale-revision-proposal",
                    "the revision proposal changed before the cutover",
                ));
            }
            append_claim_tx(
                &transaction,
                &self.origin,
                &proposal.subject,
                "revision-proposal.applied",
                Some(&actor),
                &json!({"fields": {
                    "status": "applied",
                    "successor_generation": generation_subject
                }}),
                &[],
                None,
            )
            .map_err(internal)?;
        }
        let view = mission_run_view_tx(&transaction, run_id).map_err(internal)?;
        transaction
            .execute(
                "INSERT INTO idempotency(operation_id, response) VALUES (?1, ?2)",
                params![
                    opaque_cache_key(idempotency_key),
                    serde_json::to_string(&view).map_err(internal)?
                ],
            )
            .map_err(internal)?;
        transaction.commit().map_err(internal)?;
        Ok(view)
    }

    pub fn active_mission_runs(&self) -> Result<Vec<MissionRunView>> {
        let connection = self.readers.get();
        let mut statement = connection.prepare(
            "SELECT id FROM mission_runs WHERE status IN ('running','standing','blocked') ORDER BY created_at_unix_ms",
        )?;
        let ids = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        ids.into_iter()
            .map(|id| mission_run_view_tx(&connection, &id).map_err(Into::into))
            .collect()
    }

    /// Return every materialized mission run without walking the append-only claim log.
    pub fn mission_runs(&self) -> Result<Vec<MissionRunView>> {
        let connection = self.readers.get();
        let mut statement =
            connection.prepare("SELECT id FROM mission_runs ORDER BY created_at_unix_ms, id")?;
        let ids = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        ids.into_iter()
            .map(|id| mission_run_view_tx(&connection, &id).map_err(Into::into))
            .collect()
    }

    /// Lightweight run headers for collection projections that do not render steps.
    /// Unlike `mission_runs`, this avoids per-step queue and loop history scans.
    pub fn mission_run_headers(&self) -> Result<Vec<MissionRunView>> {
        let connection = self.readers.get();
        let mut statement = connection.prepare(MISSION_RUN_HEADER_SELECT)?;
        statement
            .query_map([], mission_run_header_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// The headers of the runs still running, standing or blocked, without the finished ones.
    pub fn open_mission_run_headers(&self) -> Result<Vec<MissionRunView>> {
        let connection = self.readers.get();
        connection
            .prepare_cached(&open_mission_run_headers_query())?
            .query_map([], mission_run_header_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// The run headers of the missions `ids`, without their steps.
    pub fn mission_run_headers_for_missions(&self, ids: &[String]) -> Result<Vec<MissionRunView>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let connection = self.readers.get();
        let sql = MISSION_RUN_HEADER_SELECT.replace(
            "ORDER BY mission_runs.created_at_unix_ms, mission_runs.id",
            "WHERE mission_runs.mission_id IN (SELECT value FROM json_each(?1))
             ORDER BY mission_runs.created_at_unix_ms, mission_runs.id",
        );
        let ids = ids
            .iter()
            .map(|id| id.trim_start_matches("mission/"))
            .collect::<Vec<_>>();
        connection
            .prepare_cached(&sql)?
            .query_map([serde_json::to_string(&ids)?], mission_run_header_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// When each run entered its current state, and the outcome someone set on it, from its
    /// state claims in canonical order, so every node that holds the same claims agrees.
    pub fn mission_run_states(&self) -> Result<BTreeMap<String, MissionRunStateMoment>> {
        self.mission_run_states_among(None)
    }

    /// `mission_run_states` for the runs `subjects` alone, by their subjects' claims, so a page of
    /// missions does not read every run's state history.
    pub fn mission_run_states_for_runs(
        &self,
        subjects: &[String],
    ) -> Result<BTreeMap<String, MissionRunStateMoment>> {
        self.mission_run_states_among(Some(subjects))
    }

    pub fn mission_run_states_among(
        &self,
        subjects: Option<&[String]>,
    ) -> Result<BTreeMap<String, MissionRunStateMoment>> {
        let connection = self.readers.get();
        let filter = if subjects.is_some() {
            "claims.subject IN (SELECT value FROM json_each(?1)) AND"
        } else {
            "?1 IS NULL AND"
        };
        let mut statement = connection.prepare_cached(&format!(
            "SELECT claims.subject, claims.accepted_at_unix_ms, claims.actor,
                    json_extract(claims.body, '$.fields.status'),
                    json_extract(claims.body, '$.fields.phase'),
                    json_extract(claims.body, '$.fields.previous_phase'),
                    json_extract(claims.body, '$.fields.reason')
             FROM claims JOIN batches ON batches.id=claims.batch_id
             WHERE {filter} claims.kind='mission-run.state'
             ORDER BY claims.subject, {CANONICAL_ORDER}"
        ))?;
        let selected = subjects.map(serde_json::to_string).transpose()?;
        let mut states = BTreeMap::<String, MissionRunStateMoment>::new();
        for row in statement.query_map([selected], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<String>>(6)?,
            ))
        })? {
            let (subject, at, actor, status, phase, previous_phase, reason) = row?;
            let at = at.parse().unwrap_or_default();
            // `mission-run.state` claims are durable, so the claim before is the state replaced.
            let previous_status = states.get(&subject).and_then(|state| state.status.clone());
            let outcome = (phase.as_deref() == Some("terminal")
                && previous_phase.as_deref() == Some("terminal"))
            .then(|| {
                Some(MissionRunOutcomeView {
                    status: status.clone()?,
                    previous_status,
                    reason: reason.unwrap_or_default(),
                    actor: actor?,
                    at_unix_ms: at,
                })
            })
            .flatten();
            states.insert(
                subject,
                MissionRunStateMoment {
                    since_unix_ms: at,
                    status,
                    outcome,
                },
            );
        }
        Ok(states)
    }

    pub fn active_mission_runs_for_origin(&self, origin: &str) -> Result<Vec<MissionRunView>> {
        self.active_mission_run_ids_for_origin(origin)?
            .iter()
            .map(|id| self.mission_run_for_reconcile(id))
            .collect()
    }

    /// Live work that `seat` holds or is assigned in a run outside the root of `owner_run`.
    pub fn seat_work_in_other_runs(&self, seat: &str, owner_run: &str) -> Result<Vec<String>> {
        // Any step or run can come to hold or release this seat's work.
        smallclaims::touched::note_read(|| format!("actor:{seat}"));
        smallclaims::touched::note_read(|| "kind:step-run.state".to_owned());
        smallclaims::touched::note_read(|| "kind:mission-run.state".to_owned());
        let owner = owner_run.strip_prefix("mission-run/").unwrap_or(owner_run);
        let connection = self.readers.get();
        let mut statement = connection.prepare(
            "SELECT step_runs.subject
             FROM step_runs JOIN mission_runs ON mission_runs.id=step_runs.run_id
             WHERE (step_runs.assignee=?1 OR step_runs.lease_owner=?1)
               AND step_runs.status IN ('ready','claimed','working','verifying')
               AND step_runs.generation_id=mission_runs.current_generation_id
               AND mission_runs.status IN ('running','standing','blocked')
               AND mission_runs.phase NOT LIKE 'cleanup-%'
               AND mission_runs.phase<>'terminal'
               AND mission_runs.root_run_id<>COALESCE(
                     (SELECT root_run_id FROM mission_runs WHERE id=?2), ?2)
             ORDER BY step_runs.subject LIMIT 3",
        )?;
        let subjects = statement
            .query_map(params![seat, owner], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(subjects)
    }

    /// All-pending current generations first, newest first within each group. This puts first
    /// readiness before older runs' gate writes. Every active run remains in the finite snapshot;
    /// arrivals during a pass wait for the next pass and cannot displace its remaining work.
    /// Build views separately so a malformed run cannot hide other runs.
    pub fn active_mission_run_ids_for_origin(&self, origin: &str) -> Result<Vec<String>> {
        let connection = self.readers.get();
        let mut statement = connection.prepare(
            "SELECT mission_runs.id
             FROM mission_runs
             WHERE mission_runs.status IN ('running','standing','blocked')
               AND EXISTS (
                 SELECT 1 FROM claims
                 WHERE claims.subject='mission-run/' || mission_runs.id
                   AND claims.kind='mission-run.created'
                   AND claims.origin=?1
               )
             ORDER BY EXISTS (
                 SELECT 1 FROM step_runs
                 WHERE step_runs.generation_id=mission_runs.current_generation_id
                   AND step_runs.status<>'pending'
             ), CAST(mission_runs.created_at_unix_ms AS INTEGER) DESC, mission_runs.id DESC",
        )?;
        statement
            .query_map([origin], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// One run's view with the effective step state and definitions the reconciler needs.
    ///
    /// The presentation view scans message history for each step's wake badge. Doing that for
    /// every active run on every graph change is quadratic in fleet history and can saturate an
    /// otherwise idle daemon.
    pub fn mission_run_for_reconcile(&self, id: &str) -> Result<MissionRunView> {
        let connection = self.readers.get();
        let view = mission_run_view_for_reconcile_tx(&connection, id)?;
        note_run_view_reads(&view);
        Ok(view)
    }

    /// Hydrate the reconcile view and retain its pinned definition for evaluation.
    /// Empty runs load the definition lazily in the evaluator, after cleanup checks.
    pub fn mission_run_for_evaluation(
        &self,
        id: &str,
    ) -> Result<(MissionRunView, Option<MissionSpec>)> {
        let connection = self.readers.get();
        let (view, mission) = mission_run_view_and_definition_tx(&connection, id, false)?;
        note_run_view_reads(&view);
        Ok((view, mission))
    }

    pub fn next_active_mission_deadline(&self, origin: &str) -> Result<Option<u128>> {
        let connection = self.readers.get();
        let mut statement = connection.prepare(
            "SELECT mission_run_deadlines.deadline_at_unix_ms
             FROM mission_run_deadlines
             JOIN mission_runs ON mission_runs.id=mission_run_deadlines.run_id
             WHERE mission_runs.status IN ('running','standing','blocked')
               AND mission_runs.phase NOT LIKE 'cleanup-%'
               AND mission_run_deadlines.deadline_at_unix_ms IS NOT NULL
               AND EXISTS (
                 SELECT 1 FROM claims
                 WHERE claims.subject='mission-run/' || mission_runs.id
                   AND claims.kind='mission-run.created'
                   AND claims.origin=?1
               )",
        )?;
        let deadlines = statement
            .query_map([origin], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(deadlines
            .into_iter()
            .filter_map(|value| value.parse::<u128>().ok())
            .min())
    }

    pub fn request_mission_run_cancellation(
        &self,
        run: &str,
        reason: &str,
    ) -> Result<bool, St3Error> {
        let run = if run.starts_with("mission-run/") {
            run.to_owned()
        } else {
            format!("mission-run/{run}")
        };
        self.connection
            .batched(|transaction| -> Result<bool, St3Error> {
                let claim_ids =
                    cancel_mission_run_tx(transaction, &self.origin, &run, reason, None, now_ms())?;
                Ok(!claim_ids.is_empty())
            })
            .map_err(|error| St3Error::new("internal", error))?
    }

    pub fn terminate_mission_run_descendants(&self, run: &str, reason: &str) -> Result<bool> {
        let run = run.strip_prefix("mission-run/").unwrap_or(run);
        self.connection
            .batched(|transaction| -> Result<bool> {
                let generation: Option<String> = transaction
                    .query_row(
                        "SELECT current_generation_id FROM mission_runs WHERE id=?1",
                        [run],
                        |row| row.get(0),
                    )
                    .optional()?;
                let Some(generation) = generation else {
                    return Ok(false);
                };
                let claim_ids = cancel_descendant_mission_runs_tx(
                    transaction,
                    &self.origin,
                    &generation,
                    "daemon/runtime",
                    reason,
                    now_ms(),
                )?;
                Ok(!claim_ids.is_empty())
            })
            .map_err(anyhow::Error::msg)?
    }

    pub fn mission_run_origin(&self, run: &str) -> Result<Option<String>> {
        let subject = if run.starts_with("mission-run/") {
            run.to_owned()
        } else {
            format!("mission-run/{run}")
        };
        let connection = self.readers.get();
        connection
            .query_row(
                &canonical_sql("SELECT origin FROM claims WHERE subject=?1 AND kind='mission-run.created' ORDER BY CANONICAL_ASC(claims) LIMIT 1"),
                [subject],
                |row| row.get(0),
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn active_mission_runs_for_mission(&self, mission: &str) -> Result<Vec<MissionRunView>> {
        let mission = mission.strip_prefix("mission/").unwrap_or(mission);
        let connection = self.readers.get();
        let mut statement = connection.prepare(
            "SELECT id FROM mission_runs
             WHERE mission_id=?1 AND status IN ('running','standing','blocked')
             ORDER BY created_at_unix_ms, id",
        )?;
        let ids = statement
            .query_map([mission], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        ids.into_iter()
            .map(|id| mission_run_view_tx(&connection, &id).map_err(Into::into))
            .collect()
    }

    pub fn terminal_mission_runs(&self) -> Result<Vec<MissionRunView>> {
        let connection = self.readers.get();
        let mut statement = connection.prepare(
            "SELECT id FROM mission_runs WHERE status IN ('completed','failed','cancelled') ORDER BY created_at_unix_ms",
        )?;
        let ids = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        ids.into_iter()
            .map(|id| mission_run_view_tx(&connection, &id).map_err(Into::into))
            .collect()
    }

    pub fn mission_runs_for_root(&self, root: &str) -> Result<Vec<MissionRunView>> {
        let root = root.strip_prefix("mission-run/").unwrap_or(root);
        smallclaims::touched::note_read(|| format!("children:mission-run/{root}"));
        let connection = self.readers.get();
        let mut statement = connection.prepare(
            "SELECT id FROM mission_runs WHERE root_run_id=?1 ORDER BY created_at_unix_ms, id",
        )?;
        let ids = statement
            .query_map([root], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        ids.into_iter()
            .map(|id| mission_root_tree_run_tx(&connection, &id))
            .collect()
    }

    /// Seek one bounded page of a root graph. The legacy all-descendants reader remains for
    /// callers that explicitly need the complete graph; human presentation uses this page.
    /// A missing cursor means the named run was retired or belongs to another root.
    pub fn mission_runs_for_root_page(
        &self,
        root: &str,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Option<(Vec<MissionRunView>, Option<String>)>> {
        anyhow::ensure!((1..=50).contains(&limit), "mission root page limit must be 1 through 50");
        let root = root.strip_prefix("mission-run/").unwrap_or(root);
        let connection = self.readers.get();
        let cursor = match after {
            Some(after) => {
                let after = after.strip_prefix("mission-run/").unwrap_or(after);
                let created = connection
                    .query_row(
                        "SELECT created_at_unix_ms FROM mission_runs
                         WHERE id=?1 AND root_run_id=?2",
                        params![after, root],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()?;
                let Some(created) = created else {
                    return Ok(None);
                };
                Some((created, after.to_owned()))
            }
            None => None,
        };
        let ids = if let Some((created, id)) = cursor {
            connection
                .prepare_cached(
                    "SELECT id FROM mission_runs INDEXED BY mission_runs_root_page_index
                     WHERE root_run_id=?1 AND (created_at_unix_ms,id)>(?2,?3)
                     ORDER BY created_at_unix_ms,id LIMIT ?4",
                )?
                .query_map(params![root, created, id, limit.saturating_add(1)], |row| {
                    row.get::<_, String>(0)
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
        } else {
            connection
                .prepare_cached(
                    "SELECT id FROM mission_runs INDEXED BY mission_runs_root_page_index
                     WHERE root_run_id=?1 ORDER BY created_at_unix_ms,id LIMIT ?2",
                )?
                .query_map(params![root, limit.saturating_add(1)], |row| {
                    row.get::<_, String>(0)
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let has_more = ids.len() > limit;
        let next = has_more.then(|| ids[limit - 1].clone());
        let runs = ids
            .into_iter()
            .take(limit)
            .map(|id| mission_root_tree_run_tx(&connection, &id))
            .collect::<Result<Vec<_>>>()?;
        Ok(Some((runs, next)))
    }

    pub fn work(&self, actor: Option<&str>, include_terminal: bool) -> Result<Vec<StepRunView>> {
        self.work_at_snapshot(actor, include_terminal, now_ms())
    }

    pub fn work_at_snapshot(
        &self,
        actor: Option<&str>,
        include_terminal: bool,
        snapshot_unix_ms: u128,
    ) -> Result<Vec<StepRunView>> {
        self.work_at_snapshot_internal(actor, include_terminal, snapshot_unix_ms, true)
    }

    /// Client projections include agentless steps so mission progress remains visible.
    pub fn client_work_at_snapshot(
        &self,
        actor: Option<&str>,
        include_terminal: bool,
        snapshot_unix_ms: u128,
    ) -> Result<Vec<StepRunView>> {
        self.work_at_snapshot_internal_with_agentless(
            actor,
            include_terminal,
            snapshot_unix_ms,
            true,
            true,
            false,
        )
    }

    /// Native lease polling needs only current held claims, never list presentation or timing.
    pub fn work_for_renewal(&self, actor: &str) -> Result<Vec<StepRunView>> {
        let actor = normalize_actor(actor, "agent");
        let snapshot = now_ms();
        let connection = self.readers.get();
        let views = connection
            .prepare_cached(
                "SELECT subject, run_id, step_path, definition_hash, status, attempt, assignee, available_to, agentless, title, goals, worker_reported,
                        lease_owner, lease_incarnation, lease_expires_at_unix_ms, blocked_reason, not_before_unix_ms, created_at_unix_ms, updated_at_unix_ms, readiness_epoch, constraints
                 FROM step_runs INDEXED BY step_runs_lease_index
                 WHERE lease_owner=?1 AND agentless=0
                   AND generation_id=(SELECT current_generation_id FROM mission_runs WHERE id=step_runs.run_id)
                 ORDER BY subject",
            )?
            .query_map([&actor], step_run_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut held = Vec::with_capacity(views.len());
        for mut view in views {
            apply_effective_step_state(&connection, &mut view, snapshot)?;
            if view.work_claim_is_held_by(&actor) {
                held.push(view);
            }
        }
        Ok(held)
    }

    /// The steps of the client work list, or with `history` of the work history, that a
    /// claimant holds at the snapshot, with the status and claimant those lists show. Only a
    /// step whose lease names an owner can have a claimant, so this reads the leased steps by
    /// their index rather than reading and enriching every step the store has run. The machine
    /// list needs only who holds which step.
    pub fn client_work_claims_at_snapshot(
        &self,
        history: bool,
        snapshot_unix_ms: u128,
    ) -> Result<Vec<StepRunView>> {
        let connection = self.readers.get();
        let views = connection
            .prepare_cached(
                "SELECT subject, run_id, step_path, definition_hash, status, attempt, assignee, available_to, agentless, title, goals, worker_reported,
                        lease_owner, lease_incarnation, lease_expires_at_unix_ms, blocked_reason, not_before_unix_ms, created_at_unix_ms, updated_at_unix_ms, readiness_epoch, constraints
                 FROM step_runs INDEXED BY step_runs_lease_index
                 WHERE lease_owner IS NOT NULL
                   AND (?1 OR (generation_id=(SELECT current_generation_id FROM mission_runs
                                              WHERE id=step_runs.run_id)
                               AND status NOT IN ('pending','completed','failed','cancelled')))
                 ORDER BY subject",
            )?
            .query_map([history], step_run_from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        let mut held = Vec::new();
        for mut view in views {
            apply_effective_step_state(&connection, &mut view, snapshot_unix_ms)?;
            if !history {
                // The current work list leaves out ended steps, and a run draining for a
                // revision shows only the steps still being worked.
                let draining = connection
                    .prepare_cached("SELECT phase FROM mission_runs WHERE id=?1")?
                    .query_row(
                        [view.run.strip_prefix("mission-run/").unwrap_or(&view.run)],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()?
                    .is_some_and(|phase| phase == "revision-draining");
                if (draining
                    && !matches!(view.status.as_str(), "claimed" | "working" | "verifying"))
                    || matches!(
                        view.status.as_str(),
                        "pending" | "completed" | "failed" | "cancelled"
                    )
                {
                    continue;
                }
            }
            if view.claimant.is_some() {
                held.push(view);
            }
        }
        Ok(held)
    }

    /// Return the work fields needed by the reconciler without computing CLI-only
    /// timing and wake annotations. Those annotations scan immutable history and
    /// are intentionally too expensive for the daemon's inner control loop.
    pub fn work_for_reconcile(&self, actor: &str) -> Result<Vec<StepRunView>> {
        let work = reconcile_work_context::read(self, actor, now_ms())?;
        // A generation can add steps this seat has not read yet. Existing steps already
        // register their own subjects below; their state changes must not invalidate every
        // seat. Handoffs mark the new recipient's actor key in incremental::change_keys.
        smallclaims::touched::note_read(|| format!("actor:{actor}"));
        smallclaims::touched::note_read(|| "kind:run-generation.created".to_owned());
        // Proposals enter and leave revision-draining on the proposal subject,
        // not the run subject. Approval and cancellation receipts do not carry
        // the run, so retain these collection dependencies without another query.
        for kind in [
            "revision-proposal.created",
            "revision-proposal.approved",
            "revision-proposal.cancelled",
        ] {
            smallclaims::touched::note_read(|| format!("kind:{kind}"));
        }
        for step in &work {
            smallclaims::touched::note_read(|| step.subject.clone());
            smallclaims::touched::note_read(|| step.run.clone());
        }
        Ok(work)
    }

    /// Fetch current work without the presentation-only wake history. The
    /// reconciler can enrich just the next wake candidate for each local agent.
    pub fn work_for_reconcile_all(&self) -> Result<Vec<StepRunView>> {
        self.work_at_snapshot_internal(None, false, now_ms(), false)
    }

    /// One current-step scan for the whole roster. This avoids replaying wake
    /// history or querying the step table separately for every agent card.
    pub fn agent_work_queues(&self) -> Result<BTreeMap<String, AgentWorkQueue>> {
        crate::performance::task("agent_work_queues", || -> Result<BTreeMap<String, AgentWorkQueue>> {
        let connection = self.readers.get();
        let orders = seat_run_orders_tx(&connection, None)?;
        let rows = seat_step_rows_tx(&connection, None)?;
        let mut seats = BTreeMap::<&str, Vec<SeatStep<'_>>>::new();
        for row in &rows {
            let step = row.seat_step();
            for agent in [row.assignee.as_deref(), row.claimant.as_deref()]
                .into_iter()
                .flatten()
                .collect::<BTreeSet<_>>()
            {
                seats.entry(agent).or_default().push(step);
            }
        }
        let mut queues = BTreeMap::<String, AgentWorkQueue>::new();
        for (agent, steps) in seats {
            let order = orders.get(agent).map(Vec::as_slice).unwrap_or_default();
            let selection = seat_queue::select(agent, &steps, order);
            if selection.held.is_empty() && selection.ready.is_empty() {
                continue;
            }
            queues.insert(
                agent.to_owned(),
                AgentWorkQueue {
                    current_work_ids: selection
                        .held
                        .iter()
                        .take(AGENT_WORK_PREVIEW_LIMIT)
                        .map(|subject| (*subject).to_owned())
                        .collect(),
                    active_work_count: selection.held.len() as u64,
                    next_work_id: selection.next().map(str::to_owned),
                    upcoming_work_ids: selection
                        .ready
                        .iter()
                        .take(AGENT_WORK_PREVIEW_LIMIT)
                        .map(|subject| (*subject).to_owned())
                        .collect(),
                    queued_work_count: selection.ready.len() as u64,
                },
            );
        }
        Ok(queues)
        })
    }

    /// The live mission runs queued for every seat, in seat-queue order.
    pub fn seat_run_orders(&self) -> Result<BTreeMap<String, Vec<String>>> {
        let connection = self.readers.get();
        seat_run_orders_tx(&connection, None)
    }

    /// The live mission runs queued for one seat, in seat-queue order.
    pub fn seat_run_order(&self, agent: &str) -> Result<Vec<String>> {
        smallclaims::touched::note_read(|| format!("kind:{}", seat_queue::MOVED_CLAIM));
        let agent = normalize_seat(agent);
        let connection = self.readers.get();
        Ok(seat_run_orders_tx(&connection, Some(&agent))?
            .remove(&agent)
            .unwrap_or_default())
    }

    /// Record one move in a seat's queue. The move never touches a claim: a
    /// held step stays held, and the new order applies to the next selection.
    pub fn move_seat_queue_run(
        &self,
        request: &SeatQueueMoveRequest,
    ) -> Result<ClaimRecord, St3Error> {
        let agent = normalize_seat(&request.agent);
        let run = normalize_mission_run(&request.run);
        let placement = Placement::parse(&request.placement).ok_or_else(|| {
            St3Error::new(
                "invalid-queue-placement",
                "a queue move uses top, bottom, before, or after",
            )
        })?;
        let anchor = request.anchor.as_deref().map(normalize_mission_run);
        match (placement.needs_anchor(), anchor.as_deref()) {
            (true, None) => {
                return Err(St3Error::new(
                    "missing-queue-anchor",
                    format!("a {} move names another queued run", placement.as_str()),
                ));
            }
            (false, Some(_)) => {
                return Err(St3Error::new(
                    "unexpected-queue-anchor",
                    format!("a {} move does not name another run", placement.as_str()),
                ));
            }
            (true, Some(anchor)) if anchor == run => {
                return Err(St3Error::new(
                    "invalid-queue-anchor",
                    "a run cannot move relative to itself",
                ));
            }
            _ => {}
        }
        let mut fields = BTreeMap::from([
            ("run".into(), Value::String(run.clone())),
            ("placement".into(), Value::String(placement.as_str().into())),
        ]);
        if let Some(anchor) = anchor.as_ref() {
            fields.insert("anchor".into(), Value::String(anchor.clone()));
        }
        if let Some(reason) = request.reason.as_ref().filter(|reason| !reason.is_empty()) {
            fields.insert("reason".into(), Value::String(reason.clone()));
        }
        let input = ClaimInput {
            subject: agent.clone(),
            kind: seat_queue::MOVED_CLAIM.into(),
            actor: Some(request.actor.clone()),
            fields,
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some(request.idempotency_key.clone()),
        };
        // A completed run leaves the queue, but a retry still names the earlier operation.
        // Reappend to verify the full request digest before returning its canonical claim.
        if self
            .operation_claim(&request.idempotency_key)
            .map_err(internal)?
            .is_some()
        {
            return self.append_claim(&input);
        }
        let order = self.seat_run_order(&agent).map_err(internal)?;
        for queued in std::iter::once(&run).chain(anchor.as_ref()) {
            if !order.contains(queued) {
                return Err(St3Error::new(
                    "run-not-queued",
                    format!("`{queued}` is not queued for `{agent}`"),
                )
                .with_detail("agent", agent.clone())
                .with_detail("run", queued.clone()));
            }
        }
        self.append_claim(&input)
    }

    pub fn work_at_snapshot_internal(
        &self,
        actor: Option<&str>,
        include_terminal: bool,
        snapshot_unix_ms: u128,
        detailed: bool,
    ) -> Result<Vec<StepRunView>> {
        self.work_at_snapshot_internal_with_agentless(
            actor,
            include_terminal,
            snapshot_unix_ms,
            detailed,
            false,
            false,
        )
    }

    /// A seat's steps that have not finished, pending ones included, as the reconciler's
    /// `work_for_reconcile` reads them less the completed, failed and cancelled ones. A finished
    /// step stays finished in every view, so they are left out before reading, and the open
    /// steps index keeps the read to the fleet's open steps, not every step it ever ran.
    pub fn open_work_for_seat(&self, agent: &str) -> Result<Vec<StepRunView>> {
        self.work_at_snapshot_internal_with_agentless(
            Some(agent),
            true,
            now_ms(),
            false,
            false,
            true,
        )
    }

    pub fn work_at_snapshot_internal_with_agentless(
        &self,
        actor: Option<&str>,
        include_terminal: bool,
        snapshot_unix_ms: u128,
        detailed: bool,
        include_agentless: bool,
        open_only: bool,
    ) -> Result<Vec<StepRunView>> {
        self.work_at_snapshot_selected(
            actor,
            include_terminal,
            snapshot_unix_ms,
            detailed,
            include_agentless,
            open_only,
            None,
        )
    }

    /// [`Self::work_at_snapshot_internal_with_agentless`], of only the `selected` step subjects
    /// when given, as the published work list refolds the steps whose claims changed.
    #[allow(clippy::too_many_arguments)]
    pub fn work_at_snapshot_selected(
        &self,
        actor: Option<&str>,
        include_terminal: bool,
        snapshot_unix_ms: u128,
        detailed: bool,
        include_agentless: bool,
        open_only: bool,
        selected: Option<&BTreeSet<String>>,
    ) -> Result<Vec<StepRunView>> {
        let actor = actor.map(|value| normalize_actor(value, "agent"));
        let connection = self.readers.get();
        let views = match selected {
            None => {
                let query = if actor.is_some() {
                    seat_work_at_snapshot_query(open_only)
                } else {
                    work_at_snapshot_query(open_only)
                };
                let mut statement = connection.prepare_cached(&query)?;
                let rows = statement.query_map(
                    params![actor.as_deref(), include_terminal, include_agentless],
                    step_run_from_row,
                )?;
                rows.collect::<Result<Vec<_>, _>>()?
            }
            Some(selected) => {
                // The same filter, over the selected steps by their subject.
                let query = selected_work_at_snapshot_query(open_only);
                let mut statement = connection.prepare_cached(&query)?;
                let rows = statement.query_map(
                    params![
                        actor.as_deref(),
                        include_terminal,
                        include_agentless,
                        serde_json::to_string(selected)?
                    ],
                    step_run_from_row,
                )?;
                rows.collect::<Result<Vec<_>, _>>()?
            }
        };
        let carried_claimants = carried_claimants_tx(
            &connection,
            views
                .iter()
                .filter(|view| view.status == "ready" && view.assigned_to.is_some())
                .map(|view| view.subject.as_str()),
        )?;
        let run_ids = views
            .iter()
            .map(|view| view.run.strip_prefix("mission-run/").unwrap_or(&view.run))
            .collect::<BTreeSet<_>>();
        let run_phases = connection
            .prepare(
                "SELECT id, phase FROM mission_runs WHERE id IN (SELECT value FROM json_each(?1))",
            )?
            .query_map([serde_json::to_string(&run_ids)?], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<BTreeMap<_, _>>>()?;
        let mut visible = Vec::with_capacity(views.len());
        let mut definitions = PinnedDefinitions::default();
        for mut view in views {
            if actor.is_some() && !detailed {
                // A pending available-to step or a draining run can become visible
                // without joining a new generation. Record every candidate before
                // filtering, so its own transition invalidates the seat's work.
                smallclaims::touched::note_read(|| view.subject.clone());
                smallclaims::touched::note_read(|| view.run.clone());
                smallclaims::touched::note_read(|| view.generation.clone());
            }
            if detailed {
                enrich_step_queue_cached(&connection, &mut view, snapshot_unix_ms, &mut definitions)?;
            } else {
                enrich_step_queue_for_reconcile_cached(
                    &connection,
                    &mut view,
                    snapshot_unix_ms,
                    &mut definitions,
                )?;
            }
            view.carried_claimant = carried_claimants.get(&view.subject).cloned();
            if run_phases
                .get(view.run.strip_prefix("mission-run/").unwrap_or(&view.run))
                .is_some_and(|phase| phase == "revision-draining")
                && !matches!(view.status.as_str(), "claimed" | "working" | "verifying")
            {
                continue;
            }
            let visible_to_actor = actor.as_ref().is_none_or(|actor| {
                view.assigned_to.as_deref() == Some(actor.as_str())
                    || view.claimant.as_deref() == Some(actor.as_str())
                    || (matches!(view.status.as_str(), "ready" | "blocked")
                        && view.available_to.iter().any(|candidate| candidate == actor))
            });
            if visible_to_actor
                && (include_terminal
                    || !matches!(
                        view.status.as_str(),
                        "pending" | "completed" | "failed" | "cancelled"
                    ))
            {
                visible.push(view);
            }
        }
        Ok(visible)
    }

    pub fn work_history(&self, actor: Option<&str>) -> Result<Vec<StepRunView>> {
        self.work_history_at_snapshot(actor, now_ms())
    }

    pub fn work_history_at_snapshot(
        &self,
        actor: Option<&str>,
        snapshot_unix_ms: u128,
    ) -> Result<Vec<StepRunView>> {
        self.work_history_at_snapshot_with_agentless(actor, snapshot_unix_ms, false)
    }

    pub fn client_work_history_at_snapshot(
        &self,
        actor: Option<&str>,
        snapshot_unix_ms: u128,
    ) -> Result<Vec<StepRunView>> {
        self.work_history_at_snapshot_with_agentless(actor, snapshot_unix_ms, true)
    }

    /// One page of the client's work history, newest update first: the `limit` steps after
    /// the first `offset` that `actor` can see, and whether more follow. It reads steps in
    /// that order and enriches only those it reaches, instead of enriching every step the
    /// store has ever run to show a page of them.
    pub fn client_work_history_page_at_snapshot(
        &self,
        actor: Option<&str>,
        snapshot_unix_ms: u128,
        offset: usize,
        limit: usize,
    ) -> Result<(Vec<StepRunView>, bool)> {
        self.client_work_history_page_after(actor, snapshot_unix_ms, offset, limit, None)
    }

    pub fn client_work_history_page_after(
        &self,
        actor: Option<&str>,
        snapshot_unix_ms: u128,
        offset: usize,
        limit: usize,
        after: Option<&(u128, String)>,
    ) -> Result<(Vec<StepRunView>, bool)> {
        let actor = actor.map(|value| normalize_actor(value, "agent"));
        let connection = self.readers.get();
        // Enrichment can clear a step's claimant, through its effective state alone, but
        // never changes its assignee, its candidates or its update time. So this finds every
        // step the actor can see, in the order the history shows them, and a step it matches
        // only as claimant needs just its effective state to tell.
        let visible = |view: &StepRunView| -> rusqlite::Result<bool> {
            let Some(actor) = actor.as_deref() else {
                return Ok(true);
            };
            if view.assigned_to.as_deref() == Some(actor)
                || view.available_to.iter().any(|candidate| candidate == actor)
            {
                return Ok(true);
            }
            if view.claimant.as_deref() != Some(actor) {
                return Ok(false);
            }
            let mut effective = view.clone();
            apply_effective_step_state(&connection, &mut effective, snapshot_unix_ms)?;
            Ok(effective.claimant.as_deref() == Some(actor))
        };
        let mut statement = connection.prepare_cached(
            "SELECT subject, run_id, step_path, definition_hash, status, attempt, assignee, available_to, agentless, title, goals, worker_reported,
                    lease_owner, lease_incarnation, lease_expires_at_unix_ms, blocked_reason, not_before_unix_ms, created_at_unix_ms, updated_at_unix_ms, readiness_epoch, constraints
             FROM step_runs
             WHERE (?2 IS NULL OR length(updated_at_unix_ms)<length(?2)
                    OR (length(updated_at_unix_ms)=length(?2) AND updated_at_unix_ms<?2)
                    OR (updated_at_unix_ms=?2 AND subject>?3))
               AND (agentless=0 OR ?1 IS NULL)
               AND (?1 IS NULL OR assignee=?1 OR lease_owner=?1
                    OR EXISTS (SELECT 1 FROM json_each(step_runs.available_to) WHERE value=?1))
             ORDER BY length(updated_at_unix_ms) DESC, updated_at_unix_ms DESC, subject",
        )?;
        let rows = statement.query_map(
            params![
                actor.as_deref(),
                after.map(|key| key.0.to_string()),
                after.map(|key| key.1.as_str())
            ],
            step_run_from_row,
        )?;
        let (mut skipped, mut page) = (0, Vec::new());
        let mut definitions = PinnedDefinitions::default();
        for row in rows {
            let mut view = row?;
            if !visible(&view)? {
                continue;
            }
            if skipped < offset {
                skipped += 1;
                continue;
            }
            if page.len() == limit {
                return Ok((page, true));
            }
            enrich_step_queue_cached(&connection, &mut view, snapshot_unix_ms, &mut definitions)?;
            page.push(view);
        }
        Ok((page, false))
    }

    /// One step as the client's work history shows it, found by its subject rather than by
    /// reading and enriching every step the store has ever run.
    pub fn client_work_item_at_snapshot(
        &self,
        subject: &str,
        actor: Option<&str>,
        snapshot_unix_ms: u128,
    ) -> Result<Option<StepRunView>> {
        let actor = actor.map(|value| normalize_actor(value, "agent"));
        let connection = self.readers.get();
        let view = connection
            .prepare_cached(
                "SELECT subject, run_id, step_path, definition_hash, status, attempt, assignee, available_to, agentless, title, goals, worker_reported,
                        lease_owner, lease_incarnation, lease_expires_at_unix_ms, blocked_reason, not_before_unix_ms, created_at_unix_ms, updated_at_unix_ms, readiness_epoch, constraints
                 FROM step_runs
                 WHERE subject=?3 AND (agentless=0 OR (?1 AND ?2 IS NULL))",
            )?
            .query_row(params![true, actor.as_deref(), subject], step_run_from_row)
            .optional()?;
        let Some(mut view) = view else {
            return Ok(None);
        };
        enrich_step_queue_at(&connection, &mut view, snapshot_unix_ms)?;
        let visible_to_actor = actor.as_ref().is_none_or(|actor| {
            view.assigned_to.as_deref() == Some(actor.as_str())
                || view.claimant.as_deref() == Some(actor.as_str())
                || view.available_to.iter().any(|candidate| candidate == actor)
        });
        Ok(visible_to_actor.then_some(view))
    }

    pub fn work_history_at_snapshot_with_agentless(
        &self,
        actor: Option<&str>,
        snapshot_unix_ms: u128,
        include_agentless: bool,
    ) -> Result<Vec<StepRunView>> {
        let actor = actor.map(|value| normalize_actor(value, "agent"));
        let connection = self.readers.get();
        let mut statement = connection.prepare(
            "SELECT subject, run_id, step_path, definition_hash, status, attempt, assignee, available_to, agentless, title, goals, worker_reported,
                    lease_owner, lease_incarnation, lease_expires_at_unix_ms, blocked_reason, not_before_unix_ms, created_at_unix_ms, updated_at_unix_ms, readiness_epoch, constraints
             FROM step_runs
             WHERE (agentless=0 OR (?1 AND ?2 IS NULL))
             ORDER BY created_at_unix_ms, step_path, subject",
        )?;
        let rows = statement.query_map(
            params![include_agentless, actor.as_deref()],
            step_run_from_row,
        )?;
        let mut visible = Vec::new();
        let mut definitions = PinnedDefinitions::default();
        for row in rows {
            let mut view = row?;
            enrich_step_queue_cached(&connection, &mut view, snapshot_unix_ms, &mut definitions)?;
            let visible_to_actor = actor.as_ref().is_none_or(|actor| {
                view.assigned_to.as_deref() == Some(actor.as_str())
                    || view.claimant.as_deref() == Some(actor.as_str())
                    || view.available_to.iter().any(|candidate| candidate == actor)
            });
            if visible_to_actor {
                visible.push(view);
            }
        }
        Ok(visible)
    }

    pub fn work_annotation(&self, work: &StepRunView) -> Result<OperationalAnnotation> {
        let connection = self.readers.get();
        let owner = connection
            .query_row(
                "SELECT mission_runs.status, mission_runs.current_generation_id,
                        mission_runs.mode, run_generations.status,
                        root_runs.status, root_runs.phase, mission_runs.phase
                 FROM mission_runs
                 JOIN run_generations ON run_generations.id=?2
                 JOIN mission_runs root_runs ON root_runs.id=mission_runs.root_run_id
                 WHERE mission_runs.id=?1",
                params![
                    work.run.strip_prefix("mission-run/").unwrap_or(&work.run),
                    generation_id_from_subject(&work.generation),
                ],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                    ))
                },
            )
            .optional()?;
        Ok(Self::work_annotation_with_owner(work, owner))
    }

    /// Hydrate owner state once for a collection instead of querying each step.
    pub fn work_annotations(
        &self,
        work: &[StepRunView],
    ) -> Result<BTreeMap<String, OperationalAnnotation>> {
        let runs = work
            .iter()
            .map(|step| step.run.strip_prefix("mission-run/").unwrap_or(&step.run))
            .collect::<BTreeSet<_>>();
        let connection = self.readers.get();
        let mut statement = connection.prepare(
            "SELECT mission_runs.id, run_generations.id, mission_runs.status,
                    mission_runs.current_generation_id, mission_runs.mode,
                    run_generations.status, root_runs.status, root_runs.phase, mission_runs.phase
             FROM mission_runs
             JOIN run_generations ON run_generations.run_id=mission_runs.id
             JOIN mission_runs root_runs ON root_runs.id=mission_runs.root_run_id
             WHERE mission_runs.id IN (SELECT value FROM json_each(?1))",
        )?;
        let owners = statement
            .query_map([serde_json::to_string(&runs)?], |row| {
                Ok((
                    (row.get::<_, String>(0)?, row.get::<_, String>(1)?),
                    (
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, String>(7)?,
                        row.get::<_, String>(8)?,
                    ),
                ))
            })?
            .collect::<rusqlite::Result<BTreeMap<_, _>>>()?;
        Ok(work
            .iter()
            .map(|step| {
                let run = step.run.strip_prefix("mission-run/").unwrap_or(&step.run);
                let generation = generation_id_from_subject(&step.generation);
                let owner = owners
                    .get(&(run.to_owned(), generation.to_owned()))
                    .cloned();
                (
                    step.subject.clone(),
                    Self::work_annotation_with_owner(step, owner),
                )
            })
            .collect())
    }

    pub fn work_annotation_with_owner(
        work: &StepRunView,
        owner: Option<(String, String, String, String, String, String, String)>,
    ) -> OperationalAnnotation {
        let mut reasons: Vec<String> = Vec::new();
        if let Some((
            run_status,
            current_generation,
            mode,
            generation_status,
            root_status,
            root_phase,
            run_phase,
        )) = owner
        {
            if is_terminal_run_state(&run_status) {
                reasons.push("terminal-owner".into());
                if mode == "eval" {
                    reasons.push("eval".into());
                }
            }
            if generation_id_from_subject(&work.generation) != current_generation
                || generation_status == "superseded"
            {
                reasons.push("superseded".into());
            }
            if is_terminal_run_state(&root_status) || root_phase == "terminal" {
                reasons.push("terminal-root-owner".into());
            }
            if work.status == "pending"
                && reasons.is_empty()
                && run_phase != "terminal"
                && !run_phase.starts_with("cleanup-")
                && !root_phase.starts_with("cleanup-")
            {
                // Pending work belongs to the runtime's readiness evaluator. This names
                // that owner without claiming its dependency predicates have been checked.
                reasons.push("waiting for daemon/runtime to evaluate readiness".into());
            }
        }
        if work
            .blocked_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("lease expired"))
        {
            reasons.push("expired-lease".into());
        }
        if work.status == "blocked" && !work.blockers.is_empty() {
            reasons.push("external-blocker".into());
        }
        if matches!(work.status.as_str(), "completed" | "failed" | "cancelled")
            && reasons.is_empty()
        {
            reasons.push("terminal-work".into());
        }
        reasons.sort();
        reasons.dedup();
        let historical = reasons.iter().any(|reason| {
            matches!(
                reason.as_str(),
                "terminal-owner" | "terminal-root-owner" | "superseded" | "eval" | "terminal-work"
            )
        });
        OperationalAnnotation {
            layer: if historical { "history" } else { "current" }.into(),
            actionable: !historical
                && matches!(work.status.as_str(), "ready" | "claimed" | "working"),
            reasons,
            owner_generation: Some(work.generation.clone()),
            runtime_incarnation: work.claim_incarnation.clone(),
        }
    }

    pub fn active_step_blockers_at(
        &self,
        subject: &str,
        snapshot_unix_ms: u128,
    ) -> Result<Vec<String>> {
        let connection = self.readers.get();
        active_step_blockers_tx(&connection, &normalize_step_run(subject), snapshot_unix_ms)
            .map_err(Into::into)
    }

    pub fn step_run(&self, subject: &str) -> Result<Option<StepRunView>> {
        let subject = normalize_step_run(subject);
        let connection = self.readers.get();
        let mut view = connection
            .query_row(
                "SELECT subject, run_id, step_path, definition_hash, status, attempt, assignee, available_to, agentless, title, goals, worker_reported,
                        lease_owner, lease_incarnation, lease_expires_at_unix_ms, blocked_reason, not_before_unix_ms, created_at_unix_ms, updated_at_unix_ms, readiness_epoch, constraints
                 FROM step_runs WHERE subject=?1",
                [subject],
                step_run_from_row,
            )
            .optional()
            .map_err(anyhow::Error::from)?;
        if let Some(view) = &mut view {
            enrich_step_queue(&connection, view)?;
        }
        Ok(view)
    }

    pub fn work_action(
        &self,
        subject: &str,
        action: &str,
        request: &WorkRequest,
    ) -> Result<StepRunView, St3Error> {
        self.work_action_extending(subject, action, request, None)
    }

    /// A work action; `extend_ms` is the time an `extend` adds to the attempt's execution budget.
    pub fn work_action_extending(
        &self,
        subject: &str,
        action: &str,
        request: &WorkRequest,
        extend_ms: Option<u64>,
    ) -> Result<StepRunView, St3Error> {
        self.work_action_with_handoff(subject, action, request, extend_ms, None)
    }

    pub fn handoff_work(
        &self,
        subject: &str,
        input: &crate::model::WorkHandoffRequest,
    ) -> Result<StepRunView, St3Error> {
        self.work_action_with_handoff(
            subject,
            "release",
            &WorkRequest {
                wait_ms: None,
                actor: Some(input.actor.clone()),
                incarnation: input.incarnation.clone(),
                summary: Some(input.note.clone()),
                reason: None,
                evidence: input.evidence.clone(),
                idempotency_key: input.idempotency_key.clone(),
            },
            None,
            Some(input),
        )
    }

    pub fn work_action_with_handoff(
        &self,
        subject: &str,
        action: &str,
        request: &WorkRequest,
        extend_ms: Option<u64>,
        handoff: Option<&crate::model::WorkHandoffRequest>,
    ) -> Result<StepRunView, St3Error> {
        if action == "extend" {
            if !extend_ms.is_some_and(|by| (1..=MAX_STEP_EXTENSION_MS).contains(&by)) {
                return Err(St3Error::new(
                    "invalid-extension",
                    "an extension adds between 1 millisecond and 7 days",
                ));
            }
            if request
                .reason
                .as_deref()
                .is_none_or(|reason| reason.trim().is_empty())
            {
                return Err(St3Error::new(
                    "extension-needs-reason",
                    "an extension records why the step needs more time; pass --reason",
                ));
            }
        } else if extend_ms.is_some() {
            return Err(St3Error::new(
                "invalid-work-action",
                "only an extension adds time",
            ));
        }
        if action == "wait" {
            if !request
                .wait_ms
                .is_some_and(|by| (1..=MAX_STEP_WAIT_MS).contains(&by))
            {
                return Err(St3Error::new(
                    "invalid-wait",
                    "a wait stops the execution clock for between 1 millisecond and 12 hours",
                ));
            }
            if request
                .reason
                .as_deref()
                .is_none_or(|reason| reason.trim().is_empty())
            {
                return Err(St3Error::new(
                    "wait-needs-reason",
                    "a wait records what the step waits for; pass --reason",
                ));
            }
        } else if request.wait_ms.is_some() {
            return Err(St3Error::new(
                "invalid-work-action",
                "only a wait stops the execution clock",
            ));
        }
        let subject = normalize_step_run(subject);
        let actor = normalize_actor(
            request.actor.as_deref().ok_or_else(|| {
                St3Error::new("missing-work-actor", "a work action needs an actor")
            })?,
            "agent",
        );
        let requested_incarnation = request
            .incarnation
            .as_deref()
            .filter(|incarnation| !incarnation.is_empty())
            .ok_or_else(|| {
                St3Error::new(
                    "missing-work-incarnation",
                    "a work action must be fenced to one exact live agent incarnation",
                )
            })?;
        let now = now_ms();
        // One work action in a savepoint of the writer's next batch, answered once that batch commits.
        let committed = self
            .connection
            .batched(|transaction| -> Result<StepRunView, St3Error> {
                #[cfg(any(test, feature = "test-support"))]
                let _writer_work = WorkActionWriterProbe::start(&request.idempotency_key);
                if let Some(response) = smallclaims::store::idempotency::cached_response(transaction, &request.idempotency_key)?
                {
                    if let Some(input) = handoff {
                        adhoc_work::validate_retry(transaction, &subject, input)?;
                    }
                    return serde_json::from_str(&response).map_err(internal);
                }
                if action == "claim" && rollouts::intake_held(transaction, &actor, &subject)? {
                    return Err(St3Error::new("seat-rollout-draining", "the seat holds new work intake while its rollout drains; existing work may finish"));
                }
                let current = transaction
                    .query_row(
                        "SELECT subject, run_id, step_path, definition_hash, status, attempt, assignee, available_to, agentless, title, goals, worker_reported,
                                lease_owner, lease_incarnation, lease_expires_at_unix_ms, blocked_reason, not_before_unix_ms, created_at_unix_ms, updated_at_unix_ms, readiness_epoch, constraints
                         FROM step_runs WHERE subject=?1",
                        [&subject],
                        step_run_from_row,
                    )
                    .optional()
                    .map_err(internal)?
                    .ok_or_else(|| St3Error::new("missing-step-run", format!("step run `{subject}` does not exist")))?;
                // A claim must use the same effective readiness that reads expose:
                // an expired worker no longer owns this step, even before repair.
                let mut current = current;
                if action == "claim" {
                    apply_effective_step_state(transaction, &mut current, now).map_err(internal)?;
                }
                // A revision carries a claim into the successor generation. Its worker may still name the
                // step by the predecessor subject.
                let (subject, current) = match (action != "claim")
                    .then(|| carried_claim_successor_tx(transaction, &current, &actor))
                    .transpose()
                    .map_err(internal)?
                    .flatten()
                {
                    Some(successor) => (successor.subject.clone(), successor),
                    None => (subject, current),
                };
                let (run_status, run_phase, current_generation, generation_status, root_status, root_phase): (
                    String,
                    String,
                    String,
                    String,
                    String,
                    String,
                ) = transaction
                    .query_row(
                        "SELECT mission_runs.status, mission_runs.phase,
                                mission_runs.current_generation_id, run_generations.status,
                                root_runs.status, root_runs.phase
                         FROM mission_runs JOIN run_generations
                           ON run_generations.id=?2
                         JOIN mission_runs root_runs ON root_runs.id=mission_runs.root_run_id
                         WHERE mission_runs.id=?1",
                        params![
                            current
                                .run
                                .strip_prefix("mission-run/")
                                .unwrap_or(&current.run),
                            generation_id_from_subject(&current.generation),
                        ],
                        |row| {
                            Ok((
                                row.get(0)?,
                                row.get(1)?,
                                row.get(2)?,
                                row.get(3)?,
                                row.get(4)?,
                                row.get(5)?,
                            ))
                        },
                    )
                    .map_err(internal)?;
                if generation_id_from_subject(&current.generation) != current_generation {
                    return Err(St3Error::new(
                        "stale-run-generation",
                        format!("step run `{subject}` belongs to a superseded generation"),
                    ));
                }
                if is_terminal_run_state(&run_status)
                    || run_phase == "terminal"
                    || is_terminal_generation_state(&generation_status)
                    || is_terminal_run_state(&root_status)
                    || root_phase == "terminal"
                {
                    return Err(St3Error::new(
                        "terminal-work-owner",
                        format!("the owner of step run `{subject}` is terminal"),
                    ));
                }
                if action == "claim" && run_phase == "revision-draining" {
                    return Err(St3Error::new(
                        "run-generation-draining",
                        "the current generation is draining for a revision cutover",
                    ));
                }
                if action == "claim"
                    && current.blocked_reason.is_some()
                    && !active_step_blockers_tx(transaction, &subject, now)
                        .map_err(internal)?
                        .is_empty()
                {
                    return Err(St3Error::new(
                        "work-not-ready",
                        format!("step run `{subject}` has an unresolved external blocker"),
                    ));
                }
                if let Some(input) = handoff {
                    adhoc_work::validate_recipient(transaction, input, &current)?;
                }
                if action == "claim" && adhoc_work::pending_handoff(transaction, &current)? {
                    return Err(St3Error::new("handoff-not-acknowledged", "read the handoff note and acknowledge its message before claiming this work"));
                }
                let eligible = current.assigned_to.as_deref() == Some(actor.as_str())
                    || current
                        .available_to
                        .iter()
                        .any(|candidate| candidate == &actor);
                if current.agentless || !eligible {
                    return Err(St3Error::new(
                        "work-not-available",
                        format!("`{subject}` is not available to `{actor}`"),
                    ));
                }
                if action == "claim" {
                    let mut statement = transaction
                        .prepare(
                            "SELECT subject, generation_id, step_path FROM step_runs
                             WHERE lease_owner=?1
                               AND subject<>?2
                               AND status IN ('claimed','working','verifying','blocked')
                               AND lease_expires_at_unix_ms IS NOT NULL
                               AND CAST(lease_expires_at_unix_ms AS INTEGER)>?3
                             ORDER BY subject",
                        )
                        .map_err(internal)?;
                    let active = statement
                        .query_map(params![actor, subject, now.to_string()], |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, String>(2)?,
                            ))
                        })
                        .map_err(internal)?
                        .collect::<rusqlite::Result<Vec<_>>>()
                        .map_err(internal)?;
                    drop(statement);
                    let current_generation = generation_id_from_subject(&current.generation);
                    if let Some((active_subject, _, _)) =
                        active.into_iter().find(|(_, generation, step)| {
                            generation != current_generation
                                || !(current.step.starts_with(&format!("{step}/"))
                                    || step.starts_with(&format!("{}/", current.step)))
                        })
                    {
                        return Err(St3Error::new(
                            "agent-capacity",
                            format!(
                                "`{actor}` already owns independent work `{active_subject}`; finish or release it before claiming `{subject}`"
                            ),
                        ));
                    }
                }
                if action == "complete" && adhoc_work::is_adhoc(transaction, &current.run).map_err(internal)? && request.evidence.is_empty() {
                    return Err(St3Error::new("work-needs-evidence", "close spontaneous work with --evidence"));
                }
                if matches!(
                    current.status.as_str(),
                    "completed" | "failed" | "cancelled"
                ) {
                    return Err(St3Error::new(
                        "terminal-step-run",
                        format!("step run `{subject}` is already {}", current.status),
                    ));
                }
                if action == "claim"
                    && current.status == "ready"
                    && let Some(next) =
                        seat_order_conflict_tx(transaction, &actor, &subject).map_err(internal)?
                {
                    return Err(St3Error::new(
                        "seat-queue-order",
                        format!(
                            "`{subject}` is not the next work for `{actor}`; claim `{next}` first. A person, or an agent with queue authority for this seat, can reorder it with `st agents queue move`"
                        ),
                    )
                    .with_detail("next_work_id", next));
                }
                if action == "claim" && current.status == "ready" {
                    let mut step = current.clone();
                    enrich_step_definition(transaction, &mut step).map_err(internal)?;
                    if !fresh_context_ready_tx(transaction, &step, &actor, requested_incarnation)
                        .map_err(internal)?
                    {
                        return Err(St3Error::new(
                            "fresh-context-pending",
                            format!(
                                "`{actor}` must start a fresh harness session before claiming `{subject}`"
                            ),
                        ));
                    }
                }
                let lease_valid = current
                    .claim_expires_at_unix_ms
                    .is_some_and(|expiry| expiry > now);
                if lease_valid && current.claimant.as_deref() != Some(actor.as_str()) {
                    return Err(St3Error::new(
                        "work-already-claimed",
                        format!("step run `{subject}` has an active lease"),
                    ));
                }
                if action != "claim"
                    && (!lease_valid || current.claimant.as_deref() != Some(actor.as_str()))
                {
                    return Err(St3Error::new(
                        "work-not-claimed",
                        format!("`{actor}` does not hold an active lease for `{subject}`"),
                    ));
                }
                if lease_valid
                    && current.claimant.as_deref() == Some(actor.as_str())
                    && current.claim_incarnation.as_deref() != Some(requested_incarnation)
                {
                    return Err(St3Error::new(
                        "wrong-work-incarnation",
                        format!("another `{actor}` incarnation holds `{subject}`"),
                    ));
                }
                let effective_incarnation = current
                    .claim_incarnation
                    .clone()
                    .or_else(|| Some(requested_incarnation.to_owned()));
                let actor_incarnation = effective_incarnation.clone();
                if action == "wait" {
                    // Waits draw on one budget per attempt, so a seat that keeps declaring them
                    // cannot hide a hung step from the execution timeout. The budget is time
                    // spent waiting, not time declared: a resumed wait gives back what it did
                    // not use.
                    let spent = step_execution_clock_at(transaction, &subject, current.attempt, now, true)
                        .map_err(internal)?
                        .waited;
                    let left = u128::from(MAX_STEP_WAIT_MS).saturating_sub(spent);
                    if u128::from(request.wait_ms.unwrap_or_default()) > left {
                        return Err(St3Error::new(
                            "wait-budget-exhausted",
                            format!(
                                "this attempt has waited {} of its {} wait budget; {} is left",
                                format_wait_span(spent),
                                format_wait_span(u128::from(MAX_STEP_WAIT_MS)),
                                format_wait_span(left),
                            ),
                        ));
                    }
                }
                let (status, worker_reported, claimant, claim_incarnation, claim_expiry) = match action {
                    "claim" => {
                        if current.status != "ready" && current.status != "claimed" {
                            return Err(St3Error::new(
                                "work-not-ready",
                                format!("step run `{subject}` is {}", current.status),
                            ));
                        }
                        (
                            "claimed",
                            current.worker_reported,
                            Some(actor.clone()),
                            effective_incarnation.clone(),
                            Some(now + 600_000),
                        )
                    }
                    "renew" => (
                        current.status.as_str(),
                        current.worker_reported,
                        Some(actor.clone()),
                        effective_incarnation.clone(),
                        Some(now + 600_000),
                    ),
                    "progress" => (
                        "working",
                        current.worker_reported,
                        Some(actor.clone()),
                        effective_incarnation,
                        Some(now + 600_000),
                    ),
                    "extend" => (
                        current.status.as_str(),
                        current.worker_reported,
                        Some(actor.clone()),
                        effective_incarnation,
                        Some(now + 600_000),
                    ),
                    // A wait or a resume is a progress record that moves the execution clock.
                    "wait" | "resume" => (
                        "working",
                        current.worker_reported,
                        Some(actor.clone()),
                        effective_incarnation,
                        Some(now + 600_000),
                    ),
                    "complete" => ("verifying", true, None, None, None),
                    "fail" => ("failed", current.worker_reported, None, None, None),
                    "release" => ("ready", current.worker_reported, None, None, None),
                    _ => {
                        return Err(St3Error::new(
                            "invalid-work-action",
                            format!("work action `{action}` is not registered"),
                        ));
                    }
                };
                // Most renewals only extend the local operational lease. Publish an
                // anchor before the last replicated expiry is within five minutes, so
                // another replica never sees a normally renewed lease as expired.
                let quiet_renewal = action == "renew"
                    && request.summary.is_none()
                    && request.reason.is_none()
                    && request.evidence.is_empty();
                let last_replicated_expiry = if quiet_renewal {
                    transaction
                        .prepare_cached(&last_replicated_lease_query())
                        .map_err(internal)?
                        .query_row(
                            [&subject],
                            |row| row.get::<_, Option<u64>>(0),
                        )
                        .optional()
                        .map_err(internal)?
                        .flatten()
                        .map(u128::from)
                } else {
                    None
                };
                let publish = !quiet_renewal
                    || last_replicated_expiry.is_none_or(|expiry| expiry <= now.saturating_add(300_000));
                let readiness_epoch = current
                    .readiness_epoch
                    .saturating_add(u32::from(status == "ready" && current.status != "ready"));
                transaction
                    .execute(
                        "UPDATE step_runs SET status=?2, worker_reported=?3, lease_owner=?4, lease_incarnation=?5,
                                lease_expires_at_unix_ms=?6, blocked_reason=CASE WHEN ?10 THEN ?7 ELSE blocked_reason END, readiness_epoch=?8,
                                updated_at_unix_ms=?9 WHERE subject=?1",
                        params![subject, status, worker_reported, claimant, claim_incarnation, claim_expiry.map(|value| value.to_string()), request.reason, readiness_epoch, now.to_string(), publish],
                    )
                    .map_err(internal)?;
                renew_nested_ancestor_leases_tx(
                    transaction,
                    &current.generation,
                    &current.step,
                    &actor,
                    actor_incarnation.as_deref(),
                    now,
                )?;
                let mut body = json!({"fields": {
                    "attempt": current.attempt,
                    "status": status,
                    "summary": request.summary,
                    "reason": request.reason,
                    "worker_reported": worker_reported,
                    "claimant": claimant,
                    "claim_incarnation": claim_incarnation,
                    "claim_expires_at_unix_ms": claim_expiry,
                    "readiness_epoch": readiness_epoch
                }, "evidence": request.evidence});
                if action == "complete" {
                    body["fields"]["submitted_incarnation"] = json!(actor_incarnation);
                }
                if let Some(extend_ms) = extend_ms {
                    body["fields"]["extend_ms"] = json!(extend_ms);
                }
                match action {
                    "wait" => {
                        body["fields"]["clock"] = json!("paused");
                        body["fields"]["wait_until_unix_ms"] = json!(
                            now.saturating_add(u128::from(request.wait_ms.unwrap_or_default()))
                        );
                    }
                    "resume" => body["fields"]["clock"] = json!("running"),
                    _ => {}
                }
                if let Some(input) = handoff {
                    adhoc_work::transfer_tx(transaction, &self.origin, &current, input, &mut body)?;
                }
                let claim_kind = match action {
                    "claim" => "work.claimed",
                    "renew" => "work.renewed",
                    "progress" | "wait" | "resume" => "work.progress",
                    "complete" => "work.submitted",
                    "fail" => "work.failed",
                    "release" => "work.released",
                    "extend" => "work.extended",
                    _ => unreachable!("the work action was validated above"),
                };
                if publish {
                    append_receipt_claim_tx(
                        transaction,
                        &self.origin,
                        &subject,
                        claim_kind,
                        Some(&actor),
                        &body,
                        &request.evidence,
                        None,
                        &request.idempotency_key,
                    )
                    .map_err(claim_append_error)?;
                    transaction
                        .execute(
                            "DELETE FROM local_work_lease_renewals WHERE subject=?1",
                            [&subject],
                        )
                        .map_err(internal)?;
                } else {
                    transaction
                        .execute(
                            "INSERT INTO local_work_lease_renewals
                             (subject, attempt, lease_owner, lease_incarnation, lease_expires_at_unix_ms, updated_at_unix_ms)
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                             ON CONFLICT(subject) DO UPDATE SET attempt=excluded.attempt,
                               lease_owner=excluded.lease_owner, lease_incarnation=excluded.lease_incarnation,
                               lease_expires_at_unix_ms=excluded.lease_expires_at_unix_ms,
                               updated_at_unix_ms=excluded.updated_at_unix_ms",
                            params![subject, current.attempt, actor, requested_incarnation,
                                    claim_expiry.expect("a renewal has a lease expiry").to_string(), now.to_string()],
                        )
                        .map_err(internal)?;
                }
                let mut view = transaction.query_row(
                    "SELECT subject, run_id, step_path, definition_hash, status, attempt, assignee, available_to, agentless, title, goals, worker_reported,
                            lease_owner, lease_incarnation, lease_expires_at_unix_ms, blocked_reason, not_before_unix_ms, created_at_unix_ms, updated_at_unix_ms, readiness_epoch, constraints
                     FROM step_runs WHERE subject=?1", [&subject], step_run_from_row).map_err(internal)?;
                // A renewal only moves the lease. Its view's timing, summaries and wake fold the
                // step's whole claim history, which would hold the writer longer the older the
                // step grows, so a renewal's view is filled from a reader after COMMIT.
                if action != "renew" {
                    enrich_step_queue(transaction, &mut view).map_err(internal)?;
                }
                transaction
                    .execute(
                        "INSERT INTO idempotency(operation_id, response) VALUES (?1, ?2)",
                        params![
                            opaque_cache_key(&request.idempotency_key),
                            serde_json::to_string(&view).map_err(internal)?
                        ],
                    )
                    .map_err(internal)?;
                if !publish {
                    mark_replay_safe_receipt_tx(transaction, &request.idempotency_key).map_err(internal)?;
                }
                Ok(view)
            })
            .map_err(|error| St3Error::new("internal", error))?;
        let mut view = committed?;
        if action == "renew" {
            // An exact retry of a renewal also replays a view cached without these fields.
            self.enrich_work_response(&mut view)
                .map_err(|error| St3Error::new("store-read-failed", format!("{error:#}")))?;
        }
        Ok(view)
    }

    /// Fill a committed work acknowledgement's queue, timing, summaries and wake from a reader.
    pub fn enrich_work_response(&self, view: &mut StepRunView) -> Result<()> {
        enrich_step_queue(&self.readers.get(), view).map_err(Into::into)
    }

    pub fn set_step_state(
        &self,
        subject: &str,
        status: &str,
        reason: Option<&str>,
    ) -> Result<bool> {
        let subject = normalize_step_run(subject);
        self.connection
            .batched(|transaction| -> Result<bool> {
                let current: Option<StepStateRow> = transaction
                    .query_row(
                        "SELECT step_runs.status,
                                step_runs.generation_id=mission_runs.current_generation_id,
                                step_runs.readiness_epoch, mission_runs.status, mission_runs.phase,
                                run_generations.status, root_runs.status, root_runs.phase
                         FROM step_runs JOIN mission_runs ON mission_runs.id=step_runs.run_id
                         JOIN mission_runs root_runs ON root_runs.id=mission_runs.root_run_id
                         JOIN run_generations ON run_generations.id=step_runs.generation_id
                         WHERE step_runs.subject=?1",
                        [&subject],
                        |row| {
                            Ok((
                                row.get(0)?,
                                row.get(1)?,
                                row.get(2)?,
                                row.get(3)?,
                                row.get(4)?,
                                row.get(5)?,
                                row.get(6)?,
                                row.get(7)?,
                            ))
                        },
                    )
                    .optional()?;
                let Some((
                    current,
                    is_current,
                    current_epoch,
                    run_status,
                    run_phase,
                    generation_status,
                    root_status,
                    root_phase,
                )) = current
                else {
                    return Ok(false);
                };
                if !is_current
                    || current == status
                    || ((is_terminal_run_state(&run_status)
                        || run_phase == "terminal"
                        || is_terminal_generation_state(&generation_status)
                        || is_terminal_run_state(&root_status)
                        || root_phase == "terminal")
                        && !matches!(status, "completed" | "failed" | "cancelled"))
                {
                    return Ok(false);
                }
                let now = now_ms();
                let readiness_epoch = current_epoch.saturating_add(u32::from(status == "ready"));
                transaction.execute(
                    "UPDATE step_runs SET status=?2, blocked_reason=?3,
                            lease_owner=CASE WHEN ?2 IN ('ready','completed','failed','cancelled') THEN NULL ELSE lease_owner END,
                            lease_incarnation=CASE WHEN ?2 IN ('ready','completed','failed','cancelled') THEN NULL ELSE lease_incarnation END,
                            lease_expires_at_unix_ms=CASE WHEN ?2 IN ('ready','completed','failed','cancelled') THEN NULL ELSE lease_expires_at_unix_ms END,
                            not_before_unix_ms=CASE WHEN ?2='ready' THEN NULL ELSE not_before_unix_ms END,
                            activated_at_unix_ms=CASE WHEN ?2='ready' THEN ?4 ELSE activated_at_unix_ms END,
                            readiness_epoch=?5, updated_at_unix_ms=?4 WHERE subject=?1",
                    params![subject, status, reason, now.to_string(), readiness_epoch])?;
                let body = json!({"fields": {"status": status, "reason": reason, "readiness_epoch": readiness_epoch}});
                let claim = append_claim_tx(
                    transaction,
                    &self.origin,
                    &subject,
                    "step-run.state",
                    None,
                    &body,
                    &[],
                    None,
                )?;
                if status == "completed" {
                    arm_waiting_mission_deadline_tx(transaction, &subject, claim.accepted_at_unix_ms, false)?;
                }
                Ok(true)
            })
            .map_err(anyhow::Error::msg)?
    }

    pub fn retry_step(&self, subject: &str, reason: &str, backoff_ms: u64) -> Result<bool> {
        self.retry_step_as(subject, None, reason, backoff_ms)
    }

    pub fn retry_step_as(
        &self,
        subject: &str,
        actor: Option<&str>,
        reason: &str,
        backoff_ms: u64,
    ) -> Result<bool> {
        let subject = normalize_step_run(subject);
        self.connection
            .batched(|transaction| {
                retry_step_tx(
                    transaction,
                    &self.origin,
                    &subject,
                    actor,
                    reason,
                    backoff_ms,
                    None,
                )
            })
            .map_err(anyhow::Error::msg)?
    }

    /// A reviewer's request for changes starts a fresh attempt without a failed-step interval.
    pub fn retry_step_for_feedback(
        &self,
        subject: &str,
        reviewed_attempt: u32,
        reason: &str,
        reviewer: &str,
    ) -> Result<bool> {
        let subject = normalize_step_run(subject);
        self.connection
            .batched(|transaction| -> Result<bool> {
                let current: Option<(String, u32, String)> = transaction
                    .query_row(
                        "SELECT status, attempt, goals FROM step_runs WHERE subject=?1",
                        [&subject],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                    )
                    .optional()?;
                let Some((status, attempt, goals_json)) = current else {
                    return Ok(false);
                };
                if attempt != reviewed_attempt
                    || !matches!(status.as_str(), "verifying" | "working" | "blocked")
                    || step_owner_is_terminal_tx(transaction, &subject)?
                {
                    return Ok(false);
                }
                let mut goals: Vec<String> = serde_json::from_str(&goals_json)?;
                goals.push(format!("Reviewer feedback: {reason}"));
                let next_attempt = attempt.saturating_add(1);
                let now = now_ms();
                transaction.execute(
                    "UPDATE step_runs SET status='pending', attempt=?2, goals=?3, worker_reported=0,
                            lease_owner=NULL, lease_incarnation=NULL, lease_expires_at_unix_ms=NULL,
                            blocked_reason=?4, not_before_unix_ms=NULL, activated_at_unix_ms=NULL,
                            updated_at_unix_ms=?5 WHERE subject=?1",
                    params![
                        subject,
                        next_attempt,
                        serde_json::to_string(&goals)?,
                        reason,
                        now.to_string()
                    ],
                )?;
                let body = json!({"fields": {
                    "status": "pending", "attempt": next_attempt, "reason": reason,
                    "goals": goals, "not_before_unix_ms": null
                }});
                append_claim_tx(
                    transaction,
                    &self.origin,
                    &subject,
                    "step-run.retried",
                    Some(reviewer),
                    &body,
                    &[],
                    None,
                )?;
                Ok(true)
            })
            .map_err(anyhow::Error::msg)?
    }

    pub fn set_mission_run_state(
        &self,
        run: &str,
        status: &str,
        phase: &str,
        reason: Option<&str>,
    ) -> Result<bool> {
        self.connection
            .batched(|transaction| -> Result<bool> {
                let changed = set_mission_run_state_tx(
                    transaction,
                    &self.origin,
                    run,
                    status,
                    phase,
                    reason,
                )?;
                Ok(changed)
            })
            .map_err(anyhow::Error::msg)?
    }

    pub fn mission(
        &self,
        intent: &NormalizedIntent,
        resolved_intent: IntentInput,
    ) -> Result<MissionResponse, St3Error> {
        self.mission_at(intent, resolved_intent, None)
    }

    pub fn mission_at(
        &self,
        intent: &NormalizedIntent,
        resolved_intent: IntentInput,
        at_index: Option<u64>,
    ) -> Result<MissionResponse, St3Error> {
        self.mission_at_with_declared_agents(intent, resolved_intent, at_index, &BTreeSet::new())
    }

    pub fn mission_at_with_declared_agents(
        &self,
        intent: &NormalizedIntent,
        resolved_intent: IntentInput,
        at_index: Option<u64>,
        declared_agents: &BTreeSet<String>,
    ) -> Result<MissionResponse, St3Error> {
        let connection = self.readers.get();
        let current_index = current_index(&connection).map_err(internal)?;
        let store_index = selected_index(current_index, at_index)?;
        let mut changes = Vec::new();
        let mut tokens = BTreeMap::new();
        let mut actions = Vec::new();
        let mut blockers = Vec::new();
        let mut warnings = Vec::new();
        if let Err(error) = message_subscriptions::validate(&connection, intent, store_index) {
            blockers.push(format!("{}: {}", error.code, error.message));
        }
        if let Err(error) = crate::provenance::validate_publication(&connection, intent) {
            blockers.push(format!("{}: {}", error.code, error.message));
        }

        if intent.deprecated_syntax.contains("pty") {
            warnings.push(
                "deprecated-kdl-node: `pty {}` is temporarily accepted; use canonical `terminal {}` before the friend-ready v0"
                    .into(),
            );
        }

        for reference in &intent.document_refs {
            let Some((name, hash)) = reference.rsplit_once('@') else {
                blockers.push(format!(
                    "document `{reference}` has no selected binding; run `st documents put` first"
                ));
                continue;
            };
            let exists = connection
                .query_row(
                    "SELECT 1 FROM documents WHERE name = ?1 AND hash = ?2 AND created_index<=?3",
                    params![name, hash, store_index],
                    |_| Ok(()),
                )
                .optional()
                .map_err(internal)?
                .is_some();
            if !exists {
                blockers.push(format!(
                    "missing document `{reference}`; run `st documents put` first"
                ));
                continue;
            }
            let latest: Option<String> = connection
                .query_row(
                    &canonical_sql("SELECT documents.hash FROM documents WHERE documents.name = ?1 AND documents.created_index<=?2 ORDER BY binding_key DESC LIMIT 1"),
                    params![name, store_index],
                    |row| row.get(0),
                )
                .optional()
                .map_err(internal)?;
            if latest.as_deref() != Some(hash) {
                warnings.push(format!(
                    "`{reference}` is not the latest version of `{name}`; the requested version remains valid"
                ));
            }
        }

        let known = |subject: &str| -> Result<bool, St3Error> {
            if intent.subjects.contains_key(subject) || declared_agents.contains(subject) {
                return Ok(true);
            }
            connection
                .query_row(
                    "SELECT 1 FROM claims WHERE subject=?1 AND store_index<=?2 LIMIT 1",
                    params![subject, store_index],
                    |_| Ok(()),
                )
                .optional()
                .map(|value| value.is_some())
                .map_err(internal)
        };
        for desired in intent.subjects.values() {
            if desired.kind == "agent" {
                for grouping in crate::graph::agent_under(&desired.desired) {
                    if grouping.agent == desired.subject {
                        warnings.push(format!(
                            "agent `{}` is grouped under itself",
                            desired.subject
                        ));
                    } else if !known(&grouping.agent)? {
                        warnings.push(format!(
                            "agent `{}` is grouped under missing agent `{}`",
                            desired.subject, grouping.agent
                        ));
                    }
                }
            }
            if desired.kind == "message"
                && let Some(to) = canonical_child_string(&desired.desired, "to")
            {
                let recipient = if to == "requester" || to.contains('/') {
                    to
                } else {
                    format!("agent/{to}")
                };
                if recipient != "requester"
                    && !recipient.starts_with("person/")
                    && !known(&recipient)?
                {
                    blockers.push(format!(
                        "message `{}` references undeclared recipient `{recipient}`",
                        desired.subject
                    ));
                }
            }
            if desired.kind == "observer"
                && let Some(observer) = crate::graph::observer_spec(&desired.desired)
                && !observer.stopped
                && !known(&observer.resource)?
            {
                warnings.push(format!(
                    "observer `{}` references missing resource `{}`",
                    desired.subject, observer.resource
                ));
            }
            if desired.kind == "subscription"
                && let Some(subscription) = crate::graph::subscription_spec(&desired.desired)
                && !subscription.stopped
            {
                if crate::github_watch::standing_locator(&subscription.observer).is_none()
                    && !known(&subscription.observer)?
                {
                    warnings.push(format!(
                        "subscription `{}` references missing observer `{}`",
                        desired.subject, subscription.observer
                    ));
                }
                if subscription.delivery == "message" && !known(&subscription.to)? {
                    warnings.push(format!(
                        "subscription `{}` has missing delivery target `{}`",
                        desired.subject, subscription.to
                    ));
                }
                if subscription.delivery == "mission"
                    && let (Some(mission_id), Some(revision), Some(input)) = (
                        subscription.mission.as_deref(),
                        subscription.revision.as_deref(),
                        subscription.resource_input.as_deref(),
                    )
                {
                    let local = connection
                        .query_row(
                            "SELECT body FROM mission_revisions
                             WHERE mission_id=?1 AND revision=?2 AND created_index<=?3",
                            params![mission_id, revision, store_index],
                            |row| row.get::<_, String>(0),
                        )
                        .optional()
                        .map_err(internal)?
                        .map(|body| serde_json::from_str::<MissionSpec>(&body).map_err(internal))
                        .transpose()?;
                    let published = intent
                        .missions
                        .get(mission_id)
                        .filter(|mission| mission.revision == revision)
                        .cloned()
                        .or(local);
                    match published {
                        Some(mission)
                            if mission
                                .inputs
                                .get(input)
                                .is_none_or(|declaration| declaration.kind != MissionInputKind::Resource) => blockers.push(
                            format!(
                                "subscription `{}` uses undeclared resource input `{input}` in mission/{}@{revision}",
                                desired.subject, mission_id
                            ),
                        ),
                        None => blockers.push(format!(
                            "subscription `{}` references unpublished mission/{}@{revision}",
                            desired.subject, mission_id
                        )),
                        _ => {}
                    }
                }
            }
        }
        for mission in intent.missions.values() {
            append_review_remediation_warnings(mission, &mut warnings);
            let mut selectors = mission.work_selector.iter().cloned().collect::<Vec<_>>();
            selectors.extend(
                flatten_mission_step_specs(mission)
                    .into_iter()
                    .filter_map(|step| step.work_selector.clone()),
            );
            let mut flat = Vec::new();
            flatten_steps(mission, None, &[], &mut flat);
            let selected = flat
                .iter()
                .flat_map(|(_, selector, _)| selector_agents(selector))
                .collect::<BTreeSet<_>>();
            for agent in run_agent_constraints(mission).keys() {
                if !selected.contains(agent) {
                    warnings.push(format!(
                        "mission `{}` gives agent `{agent}` constraints, but no step selects that agent",
                        mission.subject
                    ));
                }
            }
            for selector in &selectors {
                for agent in selector_agents(selector) {
                    let owned_runtime =
                        agent
                            .strip_prefix("agent/${ST_MISSION_RUN}/")
                            .is_some_and(|local| {
                                mission.revision_owners.iter().any(|owner| {
                                    owner == &format!("agent/{local}")
                                        || owner
                                            .rsplit_once('.')
                                            .is_some_and(|(_, name)| name == local)
                                })
                            });
                    if !owned_runtime && !mission.revision_owners.contains(agent) && !known(agent)?
                    {
                        warnings.push(format!(
                            "mission `{}` references missing eligible agent `{agent}`",
                            mission.subject
                        ));
                    }
                }
            }
        }
        let grouping_edges = intent
            .subjects
            .values()
            .filter(|desired| desired.kind == "agent")
            .map(|desired| {
                (
                    desired.subject.clone(),
                    crate::graph::agent_under(&desired.desired)
                        .into_iter()
                        .map(|under| under.agent)
                        .collect::<Vec<_>>(),
                )
            })
            .collect::<BTreeMap<_, _>>();
        for start in grouping_edges.keys() {
            let mut frontier = vec![start.as_str()];
            let mut visited = BTreeSet::new();
            while let Some(agent) = frontier.pop() {
                if !visited.insert(agent) {
                    continue;
                }
                for parent in grouping_edges.get(agent).into_iter().flatten() {
                    if parent == start && agent != start {
                        warnings.push(format!("agent grouping for `{start}` contains a cycle"));
                    } else if grouping_edges.contains_key(parent) {
                        frontier.push(parent);
                    }
                }
            }
        }
        blockers.sort();
        blockers.dedup();
        warnings.sort();
        warnings.dedup();

        for (subject, desired) in &intent.subjects {
            let current = desired_row_at(&connection, subject, at_index).map_err(internal)?;
            let revision = desired_revision(desired);
            tokens.insert(
                subject.clone(),
                intent_leaves_at(&connection, subject, at_index).map_err(internal)?,
            );
            let changed = current.as_ref().is_none_or(|row| row.revision != revision);
            if changed {
                changes.push(SubjectChange {
                    subject: subject.clone(),
                    change: if current.is_some() {
                        "update"
                    } else {
                        "create"
                    }
                    .into(),
                    old_revision: current.as_ref().map(|row| row.revision.clone()),
                    new_revision: revision,
                });
            }
            if !changed && desired.member.is_none() {
                continue;
            }
            if let Some(action) = crate::suspension::publication_action(self, desired, false, || {
                Ok(current.as_ref().and_then(|row| row.member.as_deref())
                    .map(serde_json::from_str::<crate::model::MemberSpec>)
                    .transpose()?)
            })
                .map_err(internal)?
            {
                if changed || action.action == "restart" {
                    actions.push(action);
                }
            } else if desired.kind == "stop" {
                actions.push(PlannedAction {
                    subject: subject.clone(),
                    action: "stop".into(),
                    reason: "the desired state explicitly stops this member".into(),
                });
            } else if desired.member.is_some() && changed {
                actions.push(PlannedAction {
                    subject: subject.clone(),
                    action: "observe-or-start".into(),
                    reason: "the desired member is active".into(),
                });
            }
        }
        for mission in intent.missions.values() {
            let subject = mission.subject.clone();
            let current: Option<(String, String)> = connection
                .query_row(
                    "SELECT revision, claim_id FROM mission_definitions WHERE mission_id=?1",
                    [&mission.id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()
                .map_err(internal)?;
            tokens.insert(
                subject.clone(),
                current
                    .as_ref()
                    .map(|(_, claim)| vec![claim.clone()])
                    .unwrap_or_default(),
            );
            if current
                .as_ref()
                .is_some_and(|(revision, _)| revision == &mission.revision)
            {
                continue;
            }
            changes.push(SubjectChange {
                subject: subject.clone(),
                change: if current.is_some() {
                    "update"
                } else {
                    "create"
                }
                .into(),
                old_revision: current.map(|(revision, _)| revision),
                new_revision: mission.revision.clone(),
            });
            actions.push(PlannedAction {
                subject,
                action: "publish-mission".into(),
                reason: "the immutable mission revision is not published".into(),
            });
        }
        for declaration in intent.mission_runs.values() {
            prepare_mission_run_declaration(
                &connection,
                declaration,
                store_index,
                &mut tokens,
                &mut actions,
                &mut blockers,
                &mut warnings,
            )?;
        }
        for declaration in intent.planning_sessions.values() {
            prepare_planning_session_declaration(
                &connection,
                declaration,
                store_index,
                &mut tokens,
                &mut actions,
                &mut blockers,
            )?;
        }
        for refresh in &intent.resource_refreshes {
            tokens.entry(refresh.resource.clone()).or_insert(
                claim_ids_at(&connection, &refresh.resource, Some(store_index))
                    .map_err(internal)?
                    .into_iter()
                    .last()
                    .into_iter()
                    .collect(),
            );
            let exists = connection
                .query_row(
                    "SELECT 1 FROM desired WHERE subject=?1 AND kind='resource'",
                    [&refresh.resource],
                    |_| Ok(()),
                )
                .optional()
                .map_err(internal)?
                .is_some();
            if exists {
                match publication_operation_is_new(
                    &connection,
                    &refresh.resource,
                    "refresh",
                    &refresh.id,
                    refresh,
                ) {
                    Ok(true) => actions.push(PlannedAction {
                        subject: refresh.resource.clone(),
                        action: format!("refresh:{}", refresh.id),
                        reason: "the publication requests a fresh observation".into(),
                    }),
                    Ok(false) => {}
                    Err(error) => blockers.push(error.message),
                }
            } else {
                blockers.push(format!("resource `{}` does not exist", refresh.resource));
            }
        }
        for repair in &intent.replica_repairs {
            let repair_subject = format!(
                "repair/{}",
                repair
                    .record_ref
                    .strip_prefix("record/")
                    .unwrap_or(&repair.record_ref)
            );
            tokens.insert(
                repair_subject.clone(),
                claim_ids_at(&connection, &repair_subject, Some(store_index)).map_err(internal)?,
            );
            match validate_replica_repair(&connection, repair) {
                Ok(true) => actions.push(PlannedAction {
                    subject: repair_subject,
                    action: "repair-record".into(),
                    reason: "the invalid record has no matching repair".into(),
                }),
                Ok(false) => {}
                Err(error) => blockers.push(error.message),
            }
        }

        blockers.sort();
        blockers.dedup();
        warnings.sort();
        warnings.dedup();

        let declaration_diffs = declarations::diffs(
            &connection,
            intent,
            changes.iter().map(|change| change.subject.as_str()),
            Some(store_index),
        )?;
        let mut mission_provenance = intent.mission_provenance.clone();
        for (id, mission) in &intent.missions {
            if !mission_provenance.contains_key(id)
                && let Some(provenance) =
                    crate::provenance::read(&connection, id, &mission.revision).map_err(internal)?
            {
                mission_provenance.insert(id.clone(), provenance);
            }
        }
        Ok(MissionResponse {
            declaration_diffs,
            store_index,
            source_hash: intent.source_hash.clone(),
            normalized: intent.normalized.clone(),
            resolved_intent,
            changes,
            predicted_actions: actions,
            blockers,
            warnings,
            subject_tokens: tokens,
            mission_provenance,
            mission_revisions: intent
                .missions
                .values()
                .map(|mission| (mission.subject.clone(), mission.revision.clone()))
                .collect(),
        })
    }

    pub fn operational_repair_plan(&self) -> Result<OperationalRepairPlan> {
        let connection = self.readers.get();
        operational_repair_plan_tx(&connection, now_ms())
    }

    /// The fields used by live terminal fences, with the same authority decisions as `status`.
    pub fn runtime_authority(&self, subject: &str) -> Result<Option<RuntimeAuthority>> {
        smallclaims::touched::note_read(|| subject.to_owned());

        let connection = self.readers.get();
        // Eligibility, inherited fields and unknown claims must share one snapshot: a rival
        // arriving between SELECTs must not be paired with the earlier single origin.
        let connection = connection.unchecked_transaction()?;
        Self::runtime_authority_on(&connection, subject)
    }

    /// Runtime fences inside an existing read or committed-write snapshot.
    pub fn runtime_authority_on(
        connection: &Connection,
        subject: &str,
    ) -> Result<Option<RuntimeAuthority>> {
        if subject.starts_with("glass/") || subject.starts_with("arrangement/") {
            return Ok(None);
        }
        let desired = desired_row_at(connection, subject, None)?;
        let (actual, actual_origin, conflict) = match runtime_only_authority(connection, subject, i64::MAX as u64, &["status", "runtime_id", "incarnation_id", "terminal", "reachability"])? {
            Some((actual, origin)) => (Some(actual), Some(origin), false),
            None => {
                let member = desired.as_ref()
                    .and_then(|row| row.member.as_deref())
                    .and_then(|value| serde_json::from_str::<crate::model::MemberSpec>(value).ok());
                let actual = latest_actual_at(connection, subject, None)?;
                let (_, origin, conflict) = selected_actual_source_at(
                    connection, subject, None, member.as_ref().map(|member| member.host.as_str()),
                )?;
                (actual, origin, conflict)
            }
        };
        let reachability = if conflict || has_unknown_claim_at(connection, subject, None)?.is_some() {
            "indeterminate"
        } else {
            actual.as_ref()
                .and_then(|value| value.get("reachability"))
                .and_then(Value::as_str)
                .unwrap_or(if actual.is_some() {
                    "reachable"
                } else if desired.is_some() {
                    "indeterminate"
                } else {
                    "unknown"
                })
        }.to_owned();
        Ok(Some(RuntimeAuthority { actual, actual_origin, reachability }))
    }

    /// Candidate owners of a runtime resource at a snapshot. Retain historical observations:
    /// the canonical status fold, not the latest ingested claim, selects the current id.
    pub fn runtime_subjects_for_id_at(
        &self,
        runtime_id: Option<&str>,
        owner: Option<&str>,
        store_index: u64,
    ) -> Result<BTreeSet<String>> {
        let connection = self.readers.get();
        let subjects = connection.prepare_cached(&RUNTIME_ID_SUBJECTS)?
            .query_map(params![runtime_id, store_index, owner], |row| row.get::<_, String>(0))?
            .collect::<Result<BTreeSet<_>, _>>()?;
        Ok(subjects)
    }

    pub fn workspace_observation_from_host(
        &self,
        subject: &str,
        host: &str,
    ) -> Result<Option<ClaimRecord>> {
        let connection = self.readers.get();
        connection
            .prepare_cached(&format!(
                "SELECT {CLAIM_COLUMNS} FROM claims JOIN batches ON batches.id=claims.batch_id
             WHERE claims.subject=?1 AND claims.kind='workspace.observed' AND claims.origin=?2
             ORDER BY {CANONICAL_ORDER_DESC} LIMIT 1",
            ))?
            .query_row(params![subject, host], smallclaims::store::claim_from_row)
            .optional()
            .map_err(Into::into)
    }

    /// Start a stopped mission seat again on the declaration its run gave it. `expected` fences
    /// the selected stop. A seat its run already declares is returned unchanged, and a run that
    /// ended or moved to another generation since the stop cannot start it.
    pub fn start_mission_seat(
        &self,
        subject: &str,
        expected: Option<&str>,
        actor: &str,
        idempotency_key: &str,
    ) -> Result<ApplyResponse, St3Error> {
        let Some(ended) = self.declaration_ended_by_stop(subject).map_err(internal)? else {
            return Err(St3Error::new(
                "seat-not-stopped",
                format!("`{subject}` is not a stopped seat with one prior declaration"),
            ));
        };
        if let Some(expected) = expected
            && expected != ended.token
        {
            return Err(St3Error::new(
                "stale-seat-declaration",
                format!("the selected declaration of `{subject}` changed"),
            ));
        }
        let declaration = ended.declaration;
        let (Some(owner_run), Some(owner_generation)) = (
            declaration.owner_run.clone(),
            declaration.owner_generation.clone(),
        ) else {
            return Err(St3Error::new(
                "seat-not-mission-owned",
                format!("`{subject}` is not a mission seat"),
            ));
        };
        let run_id = owner_run.strip_prefix("mission-run/").unwrap_or(&owner_run);
        let (status, phase, generation) = self
            .readers
            .get()
            .query_row(
                "SELECT status, phase, 'run-generation/' || current_generation_id
                 FROM mission_runs WHERE id=?1",
                [run_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(internal)?
            .ok_or_else(|| {
                St3Error::new(
                    "mission-run-missing",
                    format!("`{subject}` belongs to `{owner_run}`, which this node does not have"),
                )
            })?;
        if is_terminal_run_state(&status)
            || !matches!(phase.as_str(), "normal" | "revision-draining")
        {
            return Err(St3Error::new(
                "mission-run-ended",
                format!("`{owner_run}` is {status} ({phase}); its seats cannot start again"),
            ));
        }
        if owner_generation != generation {
            return Err(St3Error::new(
                "mission-generation-moved",
                format!(
                    "`{owner_run}` moved to {generation} after `{subject}` stopped; its new generation declares its seats"
                ),
            ));
        }
        let heads = {
            let connection = self.readers.get();
            let transaction = connection.unchecked_transaction().map_err(internal)?;
            intent_leaves_tx(&transaction, subject).map_err(internal)?
        };
        let normalized = json!({ "agent": subject, "start": ended.token });
        let intent = NormalizedIntent {
            direct_message_registrations: BTreeSet::new(),
            schema: "st3.v1".into(),
            source_hash: canonical_hash(&normalized).map_err(internal)?,
            subjects: BTreeMap::from([(subject.to_owned(), declaration)]),
            missions: BTreeMap::new(),
            mission_provenance: BTreeMap::new(),
            mission_runs: BTreeMap::new(),
            planning_sessions: BTreeMap::new(),
            resource_refreshes: Vec::new(),
            replica_repairs: Vec::new(),
            document_refs: BTreeSet::new(),
            deprecated_syntax: BTreeSet::new(),
            normalized,
        };
        self.apply_as(
            &intent,
            &BTreeMap::from([(subject.to_owned(), heads)]),
            idempotency_key,
            Some(actor),
        )
    }

    pub fn desired_subjects_for_owner_step(&self, owner_step: &str) -> Result<Vec<DesiredSubject>> {
        smallclaims::touched::note_read(|| format!("owned-step:{owner_step}"));
        let connection = self.readers.get();
        let mut statement = connection.prepare(
            "SELECT subject, kind, body, member, owner_run, owner_generation, owner_step
             FROM desired WHERE owner_step=?1 ORDER BY subject",
        )?;
        let rows = statement.query_map([owner_step], desired_from_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// The desired subjects that the runs `owner_runs` own.
    pub fn desired_subjects_for_owner_runs(
        &self,
        owner_runs: &[String],
    ) -> Result<Vec<DesiredSubject>> {
        if owner_runs.is_empty() {
            return Ok(Vec::new());
        }
        let connection = self.readers.get();
        let mut statement = connection.prepare_cached(
            "SELECT subject, kind, body, member, owner_run, owner_generation, owner_step
             FROM desired WHERE owner_run IN (SELECT value FROM json_each(?1)) ORDER BY subject",
        )?;
        let rows = statement.query_map([serde_json::to_string(owner_runs)?], desired_from_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// The desired subjects that the steps `owner_steps` own.
    pub fn desired_subjects_for_owner_steps(
        &self,
        owner_steps: &[String],
    ) -> Result<Vec<DesiredSubject>> {
        for owner_step in owner_steps {
            smallclaims::touched::note_read(|| format!("owned-step:{owner_step}"));
        }
        if owner_steps.is_empty() {
            return Ok(Vec::new());
        }
        let connection = self.readers.get();
        let mut statement = connection.prepare_cached(
            "SELECT subject, kind, body, member, owner_run, owner_generation, owner_step
             FROM desired WHERE owner_step IN (SELECT value FROM json_each(?1)) ORDER BY subject",
        )?;
        let rows = statement.query_map([serde_json::to_string(owner_steps)?], desired_from_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn desired_subjects_for_owner_run(&self, owner_run: &str) -> Result<Vec<DesiredSubject>> {
        smallclaims::touched::note_read(|| format!("owned:{owner_run}"));
        let connection = self.readers.get();
        let mut statement = connection.prepare(
            "SELECT subject, kind, body, member, owner_run, owner_generation, owner_step
             FROM desired WHERE owner_run=?1 ORDER BY subject",
        )?;
        let rows = statement.query_map([owner_run], desired_from_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Runtime members whose owning run or generation is terminal.
    ///
    /// This is deliberately derived from the mission tables instead of relying on a
    /// cleanup-authored `stop` declaration. A terminal state may arrive through
    /// replication or an older client, and runtime cleanup must still converge.
    ///
    /// A member left in a superseded generation is not included. The successor generation
    /// re-owns each member it still declares and then retires the rest durably, so a member that
    /// survives a revision keeps running.
    pub fn terminal_owned_runtime_subjects(&self) -> Result<BTreeSet<String>> {
        let connection = self.readers.get();
        let mut statement = connection.prepare(TERMINAL_OWNED_RUNTIME_SUBJECTS)?;
        statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<BTreeSet<_>, _>>()
            .map_err(Into::into)
    }

    pub fn eval_runtime_records(&self, run: &str) -> Result<Vec<(String, bool)>> {
        let run = if run.starts_with("mission-run/") {
            run.to_owned()
        } else {
            format!("mission-run/{run}")
        };
        let run_id = run.strip_prefix("mission-run/").unwrap_or(&run);
        let connection = self.connection.write();
        let mode: Option<String> = connection
            .query_row(
                "SELECT mode FROM mission_runs WHERE id=?1",
                [run_id],
                |row| row.get(0),
            )
            .optional()?;
        anyhow::ensure!(
            mode.as_deref() == Some("eval"),
            "only eval runs have disposable runtime records"
        );

        let run_subjects = {
            let mut statement = connection.prepare(
                "SELECT id FROM mission_runs WHERE root_run_id=?1 ORDER BY created_at_unix_ms, id",
            )?;
            statement
                .query_map([run_id], |row| {
                    row.get::<_, String>(0)
                        .map(|id| format!("mission-run/{id}"))
                })?
                .collect::<Result<BTreeSet<_>, _>>()?
        };
        let mut records = BTreeMap::new();
        {
            let mut statement = connection.prepare(
                &canonical_sql("SELECT body FROM claims WHERE kind='intent.desired' ORDER BY CANONICAL_ASC(claims)"),
            )?;
            let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
            for row in rows {
                let desired: DesiredSubject = serde_json::from_str(&row?)?;
                if !desired
                    .owner_run
                    .as_ref()
                    .is_some_and(|owner| run_subjects.contains(owner))
                {
                    continue;
                }
                if let Some(member) = desired.member {
                    records.insert(member.runtime_id, member.terminal);
                }
            }
        }

        let mut gate_prefixes = run_subjects
            .iter()
            .filter_map(|subject| subject.strip_prefix("mission-run/"))
            .map(|id| format!("gate-operation/mission-run.{id}/"))
            .collect::<Vec<_>>();
        {
            let mut statement = connection.prepare(
                "SELECT run_generations.id
                 FROM run_generations
                 JOIN mission_runs ON run_generations.run_id=mission_runs.id
                 WHERE mission_runs.root_run_id=?1
                 ORDER BY run_generations.created_at_unix_ms, run_generations.id",
            )?;
            let rows = statement.query_map([run_id], |row| row.get::<_, String>(0))?;
            for row in rows {
                gate_prefixes.push(format!("gate-operation/step-run.{}.", row?));
            }
        }
        {
            let mut statement = connection.prepare(
                &canonical_sql("SELECT subject, body FROM claims WHERE kind='gate.requested' ORDER BY CANONICAL_ASC(claims)"),
            )?;
            let rows = statement.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            for row in rows {
                let (subject, body) = row?;
                if !gate_prefixes
                    .iter()
                    .any(|prefix| subject.starts_with(prefix))
                {
                    continue;
                }
                let body: Value = serde_json::from_str(&body)?;
                let fields = body.get("fields").unwrap_or(&body);
                let has_exec_runner = fields.get("runner").and_then(Value::as_str) == Some("exec")
                    || fields.get("model").and_then(Value::as_str).is_some();
                if has_exec_runner {
                    records.insert(subject.replace('/', "."), false);
                }
            }
        }
        Ok(records.into_iter().collect())
    }

    pub fn pending_observer_refresh_attempt(&self, observer: &str) -> Result<Option<String>> {
        let connection = self.readers.get();
        // The observations are listed once, not scanned for each request; see
        // `pending_subscription_mission_requests`.
        connection
            .query_row(
                &canonical_sql(
                    "SELECT json_extract(request.body, '$.fields.attempt')
                 FROM claims request
                 WHERE request.subject=?1
                   AND request.kind='observer.refresh-requested'
                   AND (
                     json_extract(request.body, '$.fields.attempt') IS NULL
                     OR json_extract(request.body, '$.fields.attempt') NOT IN (
                       SELECT json_extract(result.body, '$.fields.attempt') FROM claims result
                       WHERE result.subject=?1
                         AND result.kind IN ('observer.observed', 'observer.state')
                         AND json_extract(result.body, '$.fields.attempt') IS NOT NULL
                     )
                   )
                 ORDER BY CANONICAL_ASC(request)
                 LIMIT 1",
                ),
                [observer],
                |row| row.get(0),
            )
            .optional()
            .map_err(Into::into)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn record_resource_observation_attempt(
        &self,
        observer: &str,
        desired_revision: &str,
        attempt: Option<&str>,
        resource: &str,
        cursor: Option<&str>,
        facts: &Value,
        next_check_unix_ms: u128,
        subscriptions: &[(String, SubscriptionSpec)],
        current: Option<&(dyn Fn() -> bool + Sync)>,
        preparation: Option<(
            &resource_preparation::Fence,
            resource_preparation::Prepared<Result<resource_preparation::Inputs, St3Error>>,
        )>,
    ) -> Result<Option<ResourceObservationOutcome>, St3Error> {
        let facts = canonical_json_value(facts);
        let operation_hash = canonical_hash(&(
            observer,
            desired_revision,
            attempt,
            cursor,
            &facts,
            next_check_unix_ms,
        ))
        .map_err(internal)?;
        let idempotency_key = format!("resource-observation:{operation_hash}");
        #[cfg(any(test, feature = "test-support"))]
        let measurement = Mutex::new(None);
        #[cfg(any(test, feature = "test-support"))]
        let pending = Mutex::new(None);
        #[cfg(any(test, feature = "test-support"))]
        let enqueued = std::time::Instant::now();
        #[cfg(any(test, feature = "test-support"))]
        let canonical = preparation.is_none();
        #[cfg(any(test, feature = "test-support"))]
        let cached = std::sync::atomic::AtomicBool::new(false);
        let result = self.connection
            .batched(|transaction| -> Result<Option<ResourceObservationOutcome>, St3Error> {
                #[cfg(any(test, feature = "test-support"))]
                let _measurement = resource_preparation::WriterMeasure::new(&measurement);
                #[cfg(any(test, feature = "test-support"))]
                { *pending.lock().unwrap_or_else(PoisonError::into_inner) = Some(enqueued.elapsed()); }
                check_observer_completion(transaction, observer, desired_revision, current)?;
                let outcome = (|| {
                if let Some(response) = smallclaims::store::idempotency::cached_response(transaction, &idempotency_key)?
                {
                    #[cfg(any(test, feature = "test-support"))]
                    cached.store(true, std::sync::atomic::Ordering::Relaxed);
                    return serde_json::from_str(&response).map(Some).map_err(internal);
                }
                let resource_preparation::Inputs { active_subscriptions, normalized_observation, previous, baseline, repository_facts, legacy_collections, changed_fields, repository_changed_fields, recorded_items, discovered, observer_health_is_current, subscription_states, changed, should_record } = match preparation {
                    Some((fence, mut prepared)) => {
                        if !fence.current(self, transaction, &prepared) {
                            return Ok(None);
                        }
                        prepared.value.take().expect("validated preparation has a value")?
                    }
                    None => resource_preparation::inputs(transaction, observer, desired_revision,
                        attempt, resource, &facts, subscriptions)?,
                };
                let resource_kind = normalized_observation.get("kind").cloned()
                    .expect("a normalized resource observation has a kind");
                let facts = normalized_observation.get("facts")
                    .expect("a normalized resource observation has facts");
                let previous_object = previous.as_ref().and_then(Value::as_object);
                let current_object = facts.as_object()
                    .expect("prepared resource facts are an object");
                if !should_record {
                    return Ok(Some(ResourceObservationOutcome {
                        baseline,
                        changed_fields,
                        observation_claim: None,
                        message_subjects: Vec::new(),
                    }));
                }
                let now = write_time(transaction, &self.origin).map_err(internal)?;
                let sequence = next_replica_sequence(transaction, &self.origin).map_err(internal)?;
                let previous_hash = previous_batch_hash(transaction, &self.origin).map_err(internal)?;
                let batch_hash = batch_header_hash(&self.origin, sequence, previous_hash.as_deref(), now)
                    .map_err(internal)?;
                let batch_id = format!("batch/{}/{sequence}/{batch_hash}", self.origin);
                transaction
                    .execute(
                        "INSERT INTO batches(id, origin, replica_sequence, previous_hash, hash, accepted_at_unix_ms)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                        params![
                            batch_id,
                            self.origin,
                            sequence,
                            previous_hash,
                            batch_hash,
                            now.to_string()
                        ],
                    )
                    .map_err(internal)?;
                let mut receipt_key = Some(idempotency_key.as_str());
                let observer_predecessors = latest_claim_id_tx(transaction, observer)
                    .map_err(internal)?
                    .into_iter()
                    .collect::<Vec<_>>();
                if !observer_health_is_current {
                    append_first_receipt_claim_tx(
                        transaction,
                        &self.origin,
                        observer,
                        "observer.state",
                        None,
                        &json!({"fields": {
                            "state": "healthy",
                            "revision": desired_revision,
                        }}),
                        &observer_predecessors,
                        Some(&batch_id),
                        &mut receipt_key,
                    )
                    .map_err(internal)?;
                }
                if changed || attempt.is_some() {
                    let observer_predecessors = latest_claim_id_tx(transaction, observer)
                        .map_err(internal)?
                        .into_iter()
                        .collect::<Vec<_>>();
                    let mut observer_fields = json!({
                        "status": "healthy",
                        "revision": desired_revision,
                        "cursor": cursor,
                        "changed": changed,
                    });
                    if let Some(attempt) = attempt {
                        observer_fields
                            .as_object_mut()
                            .expect("observer fields are an object")
                            .insert("attempt".into(), Value::String(attempt.into()));
                    }
                    append_first_receipt_claim_tx(
                        transaction,
                        &self.origin,
                        observer,
                        "observer.observed",
                        None,
                        &json!({"fields": observer_fields}),
                        &observer_predecessors,
                        Some(&batch_id),
                        &mut receipt_key,
                    )
                    .map_err(internal)?;
                }
                // Builds before item resources kept every item in the repository facts. The first
                // observation without them records the repository's own facts once more.
                let observation_claim = if baseline
                    || !repository_changed_fields.is_empty()
                    || legacy_collections
                {
                    let predecessors = latest_claim_id_tx(transaction, resource)
                        .map_err(internal)?
                        .into_iter()
                        .collect::<Vec<_>>();
                    Some(
                        append_first_receipt_claim_tx(
                            transaction,
                            &self.origin,
                            resource,
                            "resource.observed",
                            None,
                            &json!({"fields": {
                                "kind": resource_kind,
                                "facts": repository_facts,
                                "observer": observer,
                                "baseline": baseline,
                                "changed_fields": repository_changed_fields,
                            }}),
                            &predecessors,
                            Some(&batch_id),
                            &mut receipt_key,
                        )
                        .map_err(internal)?,
                    )
                } else {
                    None
                };
                let mut collection_discoveries = BTreeMap::<String, Vec<(String, String)>>::new();
                let mut item_claims = Vec::new();
                let mut item_mentions = Vec::new();
                for (field, item) in discovered {
                    let DiscoveredItem {
                        subject,
                        kind,
                        facts: mut item_facts,
                        changed_fields: item_changed_fields,
                        data_types,
                        deliver,
                    } = item;
                    // An agent's checkout names the opener first. An authoring run's own pull
                    // request resource names the run when no agent was found.
                    if deliver
                        && field == "pull_requests"
                        && item_facts.get("opened_by").is_none()
                        && item_facts.get("opened_by_run").is_none()
                        && let Some(run) = authoring_pull_request_runs_tx(transaction, &item_facts)
                            .map_err(internal)?
                            .into_iter()
                            .next()
                    {
                        item_facts["opened_by_run"] = Value::String(run);
                    }
                    // A pull request item already keeps its recorded opener: its facts are laid
                    // over the recorded ones, and opener attribution keeps a named opener.
                    if kind == "vcs.issue" {
                        let normalized = normalize_resource_observation(
                            transaction,
                            &ClaimInput {
                                subject: subject.clone(),
                                kind: "resource.observed".into(),
                                actor: None,
                                fields: BTreeMap::from([
                                    ("kind".into(), Value::String(kind.into())),
                                    ("facts".into(), item_facts),
                                ]),
                                evidence: Vec::new(),
                                expected_subject: None,
                                idempotency_key: None,
                            },
                        )?
                        .expect("an issue observation is normalized");
                        item_facts = normalized["facts"].clone();
                    }
                    let predecessors = latest_claim_id_tx(transaction, &subject)
                        .map_err(internal)?
                        .into_iter()
                        .collect::<Vec<_>>();
                    let evidence = observation_claim
                        .as_ref()
                        .map(|claim| vec![claim.id.clone()])
                        .unwrap_or_default();
                    let claim = append_first_receipt_claim_tx(
                        transaction,
                        &self.origin,
                        &subject,
                        "resource.observed",
                        None,
                        &json!({"fields": {
                            "kind": kind,
                            "facts": item_facts,
                            "observer": observer,
                            "baseline": false,
                            "changed_fields": item_changed_fields,
                        }, "evidence": evidence}),
                        &predecessors,
                        Some(&batch_id),
                        &mut receipt_key,
                    )
                    .map_err(internal)?;
                    if !data_types.is_empty() {
                        let prior = recorded_items.get(&subject).cloned().flatten();
                        let mentions = if data_types.contains("mentions") {
                            new_mentions(prior.as_ref(), &item_facts)
                        } else {
                            Vec::new()
                        };
                        item_mentions.push((subject.clone(), claim.id.clone(), item_facts.clone(), mentions));
                        item_claims.push((data_types, subject.clone(), claim.id.clone(), item_facts));
                    }
                    if deliver {
                        collection_discoveries
                            .entry(field.into())
                            .or_default()
                            .push((subject, claim.id));
                    }
                }
                let mut available_subscriptions = BTreeSet::new();
                for (subscription_subject, _) in &active_subscriptions {
                    let (target_exists, status, status_changed) = subscription_states
                        .get(subscription_subject)
                        .copied()
                        .expect("an active subscription has a computed state");
                    if status_changed {
                        let predecessors = latest_claim_id_tx(transaction, subscription_subject)
                            .map_err(internal)?
                            .into_iter()
                            .collect::<Vec<_>>();
                        append_first_receipt_claim_tx(
                            transaction,
                            &self.origin,
                            subscription_subject,
                            "subscription.state",
                            None,
                            &json!({"fields": {
                                "state": status,
                                "reason": (!target_exists).then_some("the delivery target is absent"),
                            }}),
                            &predecessors,
                            Some(&batch_id),
                            &mut receipt_key,
                        )
                        .map_err(internal)?;
                    }
                    if target_exists {
                        available_subscriptions.insert(subscription_subject.clone());
                    }
                }
                let mut message_subjects = Vec::new();
                // Workflow failure deliveries share this observation transaction. Their keys
                // survive observer/subscription replacement and checkpointing as resources.
                if let Some(failures) = current_object
                    .get(crate::resource::github_workflows::PERFORMANCE_FAILURES_FIELD)
                    .and_then(Value::as_array)
                {
                    let workflow_baseline = previous_object.is_none_or(|previous| {
                        !previous.contains_key(crate::resource::github_workflows::PERFORMANCE_FAILURES_FIELD)
                    });
                    for (subject, subscription) in &active_subscriptions {
                        if subscription.delivery != "message"
                            || subscription.batch_every_ms.is_some()
                            || subscription.watch.is_some()
                            || !subscription.fields.iter().any(|field| field == crate::resource::github_workflows::PERFORMANCE_FAILURES_FIELD)
                            || subscription.condition.as_ref().is_some_and(|condition| {
                                !subscription_condition_matches(condition, &repository_facts)
                            })
                        {
                            continue;
                        }
                        if !workflow_baseline && !available_subscriptions.contains(subject) {
                            continue;
                        }
                        message_subjects.extend(github_workflow_failures::deliver_tx(
                            transaction, &self.origin, &batch_id, observer, resource,
                            subject, &subscription.to,
                            current_object.get("repository_id").unwrap_or(&Value::Null),
                            failures, workflow_baseline,
                        )?);
                    }
                }
                // Each watch on an item this observation changed hears the comments and reviews it
                // did not know, a move of the required checks into pass or fail, and the close
                // that ends it, each as one wake. The baseline tells no watch anything.
                if !baseline {
                    for (subscription_subject, subscription) in &active_subscriptions {
                        let Some(watch) = &subscription.watch else {
                            continue;
                        };
                        if !available_subscriptions.contains(subscription_subject)
                            || watch.until_unix_ms.is_some_and(|until| until <= now_ms())
                            || watch_ended_tx(transaction, subscription_subject, watch)?.is_some()
                        {
                            continue;
                        }
                        let Some((_, item_subject, item_claim, item_facts)) =
                            item_claims.iter().find(|(_, subject, ..)| {
                                subject.rsplit('/').next().and_then(|number| number.parse().ok())
                                    == Some(watch.item)
                            })
                        else {
                            continue;
                        };
                        let Some((thread, _)) = crate::github_watch::watch_parts(subscription_subject)
                        else {
                            continue;
                        };
                        let prior = recorded_items
                            .get(item_subject)
                            .cloned()
                            .flatten()
                            .map(|(facts, _)| facts);
                        let seen = REPOSITORY_COLLECTIONS
                            .iter()
                            .filter_map(|(field, _, _)| current_object.get(*field).and_then(Value::as_array))
                            .flatten()
                            .find(|item| item.get("number").and_then(Value::as_u64) == Some(watch.item))
                            .cloned()
                            .unwrap_or(Value::Null);
                        for event in crate::github_watch::watch_events(
                            watch,
                            prior.as_ref(),
                            &seen,
                            item_facts,
                        ) {
                            let posted_by = match &event {
                                crate::github_watch::WatchEvent::Comment(entry) => {
                                    github_posted_by_tx(transaction, &thread, entry)?
                                }
                                _ => None,
                            };
                            // A seat never wakes for what it posted.
                            if posted_by.as_deref() == Some(subscription.to.as_str()) {
                                continue;
                            }
                            let wake = crate::github_watch::wake(
                                subscription_subject,
                                watch,
                                &thread,
                                item_facts,
                                &event,
                                item_claim,
                                posted_by.as_deref(),
                            )?;
                            let ended = matches!(event, crate::github_watch::WatchEvent::Ended { .. });
                            if latest_claim_id_tx(transaction, &wake.subject)
                                .map_err(internal)?
                                .is_none()
                            {
                                append_first_receipt_claim_tx(
                                    transaction,
                                    &self.origin,
                                    &wake.subject,
                                    "message.sent",
                                    None,
                                    &json!({"fields": {
                                        "from": format!("daemon/{}", self.origin),
                                        "to": subscription.to,
                                        "title": wake.title,
                                        "content": wake.content,
                                        "status": "sent",
                                        "tags": wake.tags,
                                    }, "evidence": [item_claim]}),
                                    &[],
                                    Some(&batch_id),
                                    &mut receipt_key,
                                )
                                .map_err(claim_append_error)?;
                                message_subjects.push(wake.subject.clone());
                            }
                            if ended {
                                let merged = matches!(
                                    event,
                                    crate::github_watch::WatchEvent::Ended { merged: true }
                                );
                                end_watch_tx(
                                    transaction,
                                    &self.origin,
                                    Some(&batch_id),
                                    subscription_subject,
                                    watch,
                                    if merged { "merged" } else { "closed" },
                                    Some(&wake.subject),
                                )?;
                            }
                        }
                    }
                }
                if !baseline && !changed_fields.is_empty() {
                    let evidence = observation_claim
                        .as_ref()
                        .map(|claim| vec![claim.id.clone()])
                        .unwrap_or_default();
                    for (subscription_subject, subscription) in &active_subscriptions {
                        if !available_subscriptions.contains(subscription_subject)
                            || subscription.watch.is_some()
                        {
                            continue;
                        }
                        let selected = changed_fields
                            .iter()
                            .filter(|field| field.as_str() != crate::resource::github_workflows::PERFORMANCE_FAILURES_FIELD)
                            .filter(|field| subscription.fields.contains(field))
                            .cloned()
                            .collect::<Vec<_>>();
                        if selected.is_empty() {
                            continue;
                        }
                        if let Some(condition) = &subscription.condition {
                            let condition_is_true =
                                subscription_condition_matches(condition, &repository_facts);
                            let condition_was_true = previous
                                .as_ref()
                                .is_some_and(|facts| subscription_condition_matches(condition, facts));
                            if !condition_is_true || condition_was_true {
                                continue;
                            }
                        }
                        let selected_items = item_claims
                            .iter()
                            .filter(|(data_types, ..)| selected.iter().any(|name| data_types.contains(name)))
                            .collect::<Vec<_>>();
                        let stable = if selected_items.is_empty() {
                            canonical_hash(&(
                                observation_claim.as_ref().map(|claim| claim.id.as_str()),
                                subscription_subject,
                            ))
                        } else {
                            canonical_hash(&(
                                observation_claim.as_ref().map(|claim| claim.id.as_str()),
                                subscription_subject,
                                selected_items
                                    .iter()
                                    .map(|(_, _, claim, _)| claim.as_str())
                                    .collect::<Vec<_>>(),
                            ))
                        }
                        .map_err(internal)?;
                        // A batched message collects new items and mentions until the reconciler
                        // sends them together, at most once per interval.
                        if subscription.delivery == "message" && subscription.batch_every_ms.is_some() {
                            let repository_identity = current_object
                                .get("repository_id")
                                .filter(|value| !value.is_null())
                                .cloned()
                                .unwrap_or_else(|| Value::String(resource.into()));
                            let entries = batch_entries_tx(
                                transaction,
                                &self.origin,
                                &batch_id,
                                subscription_subject,
                                subscription,
                                &subscription.to,
                                &repository_identity,
                                &selected,
                                &collection_discoveries,
                                &item_claims,
                                &item_mentions,
                                &mut message_subjects,
                            )?;
                            if entries.is_empty() {
                                continue;
                            }
                            let delivery_key = canonical_hash(&(
                                "batched",
                                &subscription.to,
                                subscription_subject.rsplit('/').next(),
                                entries.iter().map(|(key, ..)| key.as_str()).collect::<Vec<_>>(),
                            ))
                            .map_err(internal)?;
                            let recorded = transaction
                                .query_row(
                                    "SELECT 1 FROM claims WHERE kind='subscription.batched'
                                     AND json_extract(body, '$.fields.delivery_key')=?1 LIMIT 1",
                                    [&delivery_key],
                                    |_| Ok(()),
                                )
                                .optional()
                                .map_err(internal)?
                                .is_some();
                            if !recorded {
                                append_first_receipt_claim_tx(
                                    transaction,
                                    &self.origin,
                                    subscription_subject,
                                    "subscription.batched",
                                    None,
                                    &json!({"fields": {
                                        "entries": entries.iter().map(|(_, _, entry)| entry).collect::<Vec<_>>(),
                                        "delivery_key": delivery_key,
                                    }, "evidence": entries.iter().map(|(_, claim, _)| claim).collect::<Vec<_>>()}),
                                    &[],
                                    Some(&batch_id),
                                    &mut receipt_key,
                                )
                                .map_err(claim_append_error)?;
                            }
                            continue;
                        }
                        // A batched delivery reaches one agent per observation, never a person
                        // directly: one request carries every new pull request head, new issue,
                        // and new mention of the named logins on items no live agent owns, so the
                        // mission's agent can group, summarize, and ask once. An owned item goes
                        // to its owner.
                        if subscription.delivery == "mission"
                            && let Some(text_input) = subscription.text_input.as_deref()
                        {
                            let (Some(mission), Some(resource_input), Some(workspace)) = (
                                subscription.mission.as_deref(),
                                subscription.resource_input.as_deref(),
                                subscription.workspace.as_deref(),
                            ) else {
                                continue;
                            };
                            let repository_identity = current_object
                                .get("repository_id")
                                .filter(|value| !value.is_null())
                                .cloned()
                                .unwrap_or_else(|| Value::String(resource.into()));
                            let delivery_scope = subscription_subject
                                .rsplit('/')
                                .next()
                                .expect("a subscription subject has a local name");
                            let unowned = batch_entries_tx(
                                transaction,
                                &self.origin,
                                &batch_id,
                                subscription_subject,
                                subscription,
                                mission,
                                &repository_identity,
                                &selected,
                                &collection_discoveries,
                                &item_claims,
                                &item_mentions,
                                &mut message_subjects,
                            )?;
                            if unowned.is_empty() {
                                continue;
                            }
                            let delivery_key = canonical_hash(&(
                                "batch",
                                mission,
                                delivery_scope,
                                unowned.iter().map(|(key, ..)| key.as_str()).collect::<Vec<_>>(),
                            ))
                            .map_err(internal)?;
                            let requested = transaction
                                .query_row(
                                    "SELECT 1 FROM claims WHERE kind='subscription.mission-requested'
                                     AND json_extract(body, '$.fields.delivery_key')=?1 LIMIT 1",
                                    [&delivery_key],
                                    |_| Ok(()),
                                )
                                .optional()
                                .map_err(internal)?
                                .is_some();
                            // The run pins the repository's latest observation as its source.
                            let Some(discovery) = observation_claim
                                .as_ref()
                                .map(|claim| claim.id.clone())
                                .or(latest_claim_id_tx(transaction, resource).map_err(internal)?)
                            else {
                                continue;
                            };
                            if requested {
                                continue;
                            }
                            let mut request_fields = json!({
                                "mission": format!("mission/{mission}"),
                                "resource": resource,
                                "resource_input": resource_input,
                                "workspace": workspace,
                                "discovery": discovery,
                                "delivery_key": delivery_key,
                                "text_input": text_input,
                                "text": serde_json::to_string(
                                    &unowned.iter().map(|(_, _, mention)| mention).collect::<Vec<_>>()
                                ).map_err(internal)?,
                            });
                            if let Some(revision) = subscription.revision.as_deref() {
                                request_fields["mission_revision"] = Value::String(revision.into());
                            }
                            if let Some(requester) = subscription.requester.as_deref() {
                                request_fields["requester"] = Value::String(requester.into());
                            }
                            append_first_receipt_claim_tx(
                                transaction,
                                &self.origin,
                                subscription_subject,
                                "subscription.mission-requested",
                                None,
                                &json!({"fields": request_fields, "evidence": unowned
                                    .iter()
                                    .map(|(_, claim, _)| claim.clone())
                                    .collect::<Vec<_>>()}),
                                &[],
                                Some(&batch_id),
                                &mut receipt_key,
                            )
                            .map_err(claim_append_error)?;
                            continue;
                        }
                        if subscription.delivery == "mission" {
                            let Some(mission) = subscription.mission.as_deref() else {
                                continue;
                            };
                            let Some(resource_input) = subscription.resource_input.as_deref() else {
                                continue;
                            };
                            let Some(workspace) = subscription.workspace.as_deref() else {
                                continue;
                            };
                            let uses_collection = selected
                                .iter()
                                .any(|field| matches!(field.as_str(), "pull_requests" | "issues"));
                            let discoveries = if uses_collection {
                                selected
                                    .iter()
                                    .flat_map(|field| {
                                        collection_discoveries
                                            .get(field)
                                            .into_iter()
                                            .flatten()
                                            .cloned()
                                    })
                                    .collect::<Vec<_>>()
                            } else {
                                observation_claim
                                    .as_ref()
                                    .map(|claim| vec![(resource.to_owned(), claim.id.clone())])
                                    .unwrap_or_default()
                            };
                            for (delivery_resource, discovery) in discoveries {
                                let discovery_body = transaction
                                    .query_row("SELECT body FROM claims WHERE id=?1", [&discovery], |row| {
                                        row.get::<_, String>(0)
                                    })
                                    .map_err(internal)?;
                                let discovery_body: Value =
                                    serde_json::from_str(&discovery_body).map_err(internal)?;
                                let head = discovery_body
                                    .pointer("/fields/facts/head_sha")
                                    .and_then(Value::as_str);
                                let number = discovery_body
                                    .pointer("/fields/facts/number")
                                    .and_then(Value::as_u64);
                                let kind = discovery_body
                                    .pointer("/fields/kind")
                                    .and_then(Value::as_str);
                                let repository_identity = current_object
                                    .get("repository_id")
                                    .filter(|value| !value.is_null())
                                    .cloned()
                                    .unwrap_or_else(|| Value::String(resource.into()));
                                let delivery_scope = subscription_subject
                                    .rsplit('/')
                                    .next()
                                    .expect("a subscription subject has a local name");
                                let delivery_key = if uses_collection {
                                    canonical_hash(&(
                                        mission,
                                        delivery_scope,
                                        repository_identity,
                                        kind,
                                        number,
                                        head,
                                    ))
                                } else {
                                    canonical_hash(&(subscription_subject, &discovery))
                                }
                                .map_err(internal)?;
                                if uses_collection
                                    && (collection_delivery_was_requested_tx(
                                        transaction,
                                        mission,
                                        delivery_scope,
                                        &delivery_resource,
                                        head,
                                        &delivery_key,
                                    )
                                    .map_err(internal)?
                                        || routed_to_owner_tx(transaction, &delivery_key)?)
                                {
                                    continue;
                                }
                                // An item that a live agent owns goes to that agent, and no
                                // review, triage, or person sees it.
                                let item_facts = &discovery_body["fields"]["facts"];
                                if uses_collection
                                    && subscription.owner_message
                                    && let Some((owner, why)) =
                                        item_owner_tx(transaction, item_facts).map_err(internal)?
                                {
                                    let (title, detail) = item_route_text(item_facts);
                                    message_subjects.extend(route_item_to_owner_tx(
                                        transaction,
                                        &self.origin,
                                        &batch_id,
                                        subscription_subject,
                                        &delivery_resource,
                                        &owner,
                                        &why,
                                        &title,
                                        &detail,
                                        &delivery_key,
                                        std::slice::from_ref(&discovery),
                                    )?);
                                    continue;
                                }
                                let mut request_fields = json!({
                                    "mission": format!("mission/{mission}"),
                                    "resource": delivery_resource,
                                    "resource_input": resource_input,
                                    "workspace": workspace,
                                    "discovery": discovery,
                                    "delivery_key": delivery_key,
                                });
                                if let Some(revision) = subscription.revision.as_deref() {
                                    request_fields
                                        .as_object_mut()
                                        .expect("subscription request fields are an object")
                                        .insert("mission_revision".into(), Value::String(revision.into()));
                                }
                                if let Some(requester) = subscription.requester.as_deref() {
                                    request_fields
                                        .as_object_mut()
                                        .expect("subscription request fields are an object")
                                        .insert("requester".into(), Value::String(requester.into()));
                                }
                                // A request cites the item version it was made for, so a checkpoint
                                // keeps that version while the request needs it.
                                let evidence = if uses_collection {
                                    vec![discovery.clone()]
                                } else {
                                    evidence.clone()
                                };
                                append_first_receipt_claim_tx(
                                    transaction,
                                    &self.origin,
                                    subscription_subject,
                                    "subscription.mission-requested",
                                    None,
                                    &json!({"fields": request_fields, "evidence": evidence}),
                                    &[],
                                    Some(&batch_id),
                                    &mut receipt_key,
                                )
                                .map_err(claim_append_error)?;
                            }
                            continue;
                        }
                        let message_subject = format!("message/resource-{}", &stable[..20]);
                        let mut content = json!({
                            "resource": resource,
                            "observer": observer,
                            "changed_fields": selected,
                            "facts": repository_facts,
                        });
                        // A message names the items that changed, up to a bound; each item's
                        // resource holds the rest.
                        if !selected_items.is_empty() {
                            content["items"] = Value::Array(
                                selected_items
                                    .iter()
                                    .take(MESSAGE_ITEMS)
                                    .map(|(_, subject, _, facts)| {
                                        json!({"resource": subject, "facts": facts})
                                    })
                                    .collect(),
                            );
                            content["items_changed"] = Value::from(selected_items.len());
                        }
                        let content = serde_json::to_string(&content).map_err(internal)?;
                        let evidence = evidence
                            .iter()
                            .cloned()
                            .chain(
                                selected_items
                                    .iter()
                                    .take(MESSAGE_ITEMS)
                                    .map(|(_, _, claim, _)| claim.clone()),
                            )
                            .collect::<Vec<_>>();
                        append_first_receipt_claim_tx(
                            transaction,
                            &self.origin,
                            &message_subject,
                            "message.sent",
                            None,
                            &json!({"fields": {
                                "from": format!("daemon/{}", self.origin),
                                "to": subscription.to,
                                "title": format!("Resource changed: {resource}"),
                                "content": content,
                                "status": "sent",
                                "tags": ["resource-change", subscription_subject],
                            }, "evidence": evidence}),
                            &[],
                            Some(&batch_id),
                            &mut receipt_key,
                        )
                        .map_err(claim_append_error)?;
                        message_subjects.push(message_subject);
                    }
                }
                let outcome = ResourceObservationOutcome {
                    baseline,
                    changed_fields,
                    observation_claim: observation_claim.map(|claim| claim.id),
                    message_subjects,
                };
                transaction
                    .execute(
                        "INSERT INTO idempotency(operation_id, response) VALUES (?1, ?2)",
                        params![
                            opaque_cache_key(&idempotency_key),
                            serde_json::to_string(&outcome).map_err(internal)?
                        ],
                    )
                    .map_err(internal)?;
                Ok(Some(outcome))
                })()?;
                check_observer_completion(transaction, observer, desired_revision, current)?;
                Ok(outcome)
            })
            .map_err(|error| St3Error::new("internal", error));
        #[cfg(any(test, feature = "test-support"))]
        if let Some((elapsed, work)) = measurement
            .into_inner()
            .unwrap_or_else(PoisonError::into_inner)
        {
            resource_preparation::COUNTS
                .with(|counts| counts.borrow_mut().writer.push((canonical, elapsed, work)));
        }
        #[cfg(any(test, feature = "test-support"))]
        resource_preparation::COUNTS.with(|counts| {
            let mut counts = counts.borrow_mut();
            if cached.load(std::sync::atomic::Ordering::Relaxed) {
                counts.cached += 1;
            } else if !canonical && matches!(&result, Ok(Ok(Some(_)))) {
                counts.fast += 1;
            }
            counts.acknowledged.push(enqueued.elapsed());
            counts
                .until_callback
                .push(pending.into_inner().unwrap_or_else(PoisonError::into_inner));
        });
        result?
    }

    pub fn authoring_review_owner(&self, discovery: &str) -> Result<Option<String>> {
        let Some(claim) = self.claim_by_id(discovery)? else {
            return Ok(None);
        };
        let Some(facts) = claim.body.pointer("/fields/facts") else {
            return Ok(None);
        };
        let candidates = authoring_pull_request_runs_tx(&self.readers.get(), facts)?;
        for run in candidates {
            let resource = format!("resource/{run}/pull-request");
            let run_id = run.strip_prefix("mission-run/").unwrap_or(&run);
            let Some(run) = self.mission_run(run_id)? else {
                continue;
            };
            let mission_id = run.mission.strip_prefix("mission/").unwrap_or(&run.mission);
            let Some(mission) = self.mission_spec(mission_id, Some(&run.revision))? else {
                continue;
            };
            let has_own_review = mission.steps.values().any(|step| {
                step.gates.iter().any(|gate| match gate {
                    crate::model::GateSpec::Human { review_targets, .. } => review_targets
                        .iter()
                        .any(|target| target == &resource || target.ends_with("/pull-request")),
                    _ => false,
                })
            });
            if has_own_review {
                return Ok(Some(format!("mission-run/{run_id}")));
            }
        }
        Ok(None)
    }

    pub fn pending_schedule_work_requests(&self, subject: &str) -> Result<Vec<ClaimRecord>> {
        let connection = self.readers.get();
        let mut statement = connection.prepare(&canonical_sql(
            "SELECT request.id, request.store_index, request.batch_id, request.subject,
                    request.kind, request.origin, request.actor, request.body,
                    request.predecessors, request.accepted_at_unix_ms
             FROM claims AS request
             WHERE request.subject=?1 AND request.kind='schedule.work-requested'
               AND request.id NOT IN (
                 SELECT json_extract(closed.body, '$.fields.request') FROM claims AS closed
                 WHERE closed.subject=?1
                   AND closed.kind IN ('schedule.work-started','schedule.work-failed')
                   AND json_extract(closed.body, '$.fields.request') IS NOT NULL
               )
             ORDER BY CANONICAL_ASC(request)",
        ))?;
        statement
            .query_map([subject], claim_from_row)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub fn schedule_work_start_for_request(
        &self,
        subject: &str,
        request: &str,
    ) -> Result<Option<ClaimRecord>> {
        let connection = self.readers.get();
        connection
            .query_row(
                &canonical_sql(
                    "SELECT id, store_index, batch_id, subject, kind, origin, actor, body,
                        predecessors, accepted_at_unix_ms
                 FROM claims WHERE subject=?1 AND kind='schedule.work-started'
                   AND json_extract(body, '$.fields.request')=?2
                 ORDER BY CANONICAL_DESC(claims) LIMIT 1",
                ),
                params![subject, request],
                claim_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn schedule_has_active_started_run(&self, subject: &str) -> Result<bool> {
        let connection = self.readers.get();
        connection
            .query_row(
                Self::SCHEDULE_ACTIVE_STARTED_RUN_QUERY,
                [subject],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    /// The earliest and latest acceptance times of the claims about `subject` that belong to one
    /// runtime: those that name `incarnation`, or, for a runtime without one, those that name
    /// `runtime`. Only claims at or before `through` count, and when `through_claim` names one of
    /// the subject's claims, only those up to it. An incarnation takes two index seeks however
    /// many claims the subject has; a runtime without one reads the subject's claims.
    pub fn runtime_claim_span_at(
        &self,
        subject: &str,
        incarnation: Option<&str>,
        runtime: Option<&str>,
        through: u64,
        through_claim: Option<&str>,
    ) -> Result<Option<(u128, u128)>> {
        let connection = self.readers.get();
        let through = match through_claim {
            Some(claim) => connection
                .prepare_cached("SELECT store_index FROM claims WHERE id=?1")?
                .query_row([claim], |row| row.get::<_, u64>(0))
                .optional()?
                .map_or(through, |index| index.min(through)),
            None => through,
        };
        let accepted = |text: String| text.parse::<u128>().unwrap_or_default();
        if let Some(incarnation) = incarnation {
            let mut span = [None, None];
            for (slot, order) in span.iter_mut().zip(["ASC", "DESC"]) {
                *slot = connection
                    .prepare_cached(&incarnation_claim_time_query(order))?
                    .query_row(params![subject, incarnation, through], |row| {
                        row.get::<_, String>(0)
                    })
                    .optional()?
                    .map(accepted);
            }
            return Ok(span[0].zip(span[1]));
        }
        let Some(runtime) = runtime else {
            return Ok(None);
        };
        let mut statement = connection.prepare_cached(
            "SELECT accepted_at_unix_ms FROM claims
             WHERE subject=?1 AND store_index<=?3
               AND json_extract(body, CASE WHEN json_type(body, '$.fields') IS NULL
                   THEN '$.runtime_id' ELSE '$.fields.runtime_id' END)=?2",
        )?;
        let mut span = None::<(u128, u128)>;
        for time in statement.query_map(params![subject, runtime, through], |row| {
            row.get::<_, String>(0)
        })? {
            let time = accepted(time?);
            span = Some(span.map_or((time, time), |(first, last)| {
                (first.min(time), last.max(time))
            }));
        }
        Ok(span)
    }

    pub fn agent_working_since(
        &self,
        agent: &str,
        incarnation: &str,
        snapshot_index: u64,
    ) -> Result<Option<u128>> {
        let connection = self.readers.get();
        agent_working_since_at(&connection, agent, incarnation, snapshot_index)
    }

    /// Read just the newest runtime body of each origin in canonical order at a snapshot.
    pub fn runtime_observations_at(
        &self, subject: &str, at: u64,
    ) -> Result<BTreeMap<String, ClaimRecord>> {
        smallclaims::touched::note_read(|| subject.to_owned());
        let connection = self.readers.get();
        let mut statement = connection.prepare_cached(&format!(
            "SELECT claims.id, claims.store_index, claims.batch_id, claims.subject,
                    claims.kind, claims.origin, claims.actor, claims.body,
                    claims.predecessors, claims.accepted_at_unix_ms
             FROM claims JOIN batches ON batches.id=claims.batch_id
             WHERE claims.subject=?1 AND claims.kind='runtime.observed' AND claims.store_index<=?2
             ORDER BY {CANONICAL_ORDER_DESC}"
        ))?;
        let mut rows = statement.query(params![subject, at.min(i64::MAX as u64)])?;
        let mut latest = BTreeMap::new();
        while let Some(row) = rows.next()? {
            let origin: String = row.get(5)?;
            if let std::collections::btree_map::Entry::Vacant(entry) = latest.entry(origin) {
                entry.insert(claim_from_row(row)?);
            }
        }
        Ok(latest)
    }

    /// Returns true only when durable runtime evidence proves that a claimed work incarnation is
    /// no longer the actor's live incarnation. Absence of runtime evidence is unknown, not death.
    pub fn work_claim_is_orphaned(&self, work: &StepRunView) -> Result<bool> {
        let (Some(actor), Some(incarnation)) =
            (work.claimant.as_deref(), work.claim_incarnation.as_deref())
        else {
            return Ok(false);
        };
        smallclaims::touched::note_read(|| actor.to_owned());
        let connection = self.readers.get();
        let mut statement = connection.prepare(&canonical_sql(
            "SELECT body FROM claims
             WHERE subject=?1 AND kind='runtime.observed'
             ORDER BY CANONICAL_DESC(claims)",
        ))?;
        let bodies = statement.query_map([actor], |row| row.get::<_, String>(0))?;
        for body in bodies {
            let body: Value = serde_json::from_str(&body?)?;
            let fields = body.get("fields").unwrap_or(&body);
            match fields.get("status").and_then(Value::as_str) {
                Some("running") => {
                    if let Some(current) = fields.get("incarnation_id").and_then(Value::as_str) {
                        return Ok(current != incarnation);
                    }
                }
                Some("exited" | "vanished" | "stopped" | "absent" | "failed") => {
                    return Ok(true);
                }
                _ => {}
            }
        }
        Ok(false)
    }

    /// Once per store, before a replay from nothing first decides its graph. Earlier replays
    /// re-applied claims over the rows a projection had left and could re-apply a run's old
    /// terminal state over the revision that reopened it, so this node's reconciler treated the
    /// run as over although its claims say it runs. A replay that follows the claims would start
    /// that work again long after anyone expected it. Each such run this node drives gets the
    /// claims that end it as its graph showed; runs that other nodes drive are theirs to settle.
    /// A run with a peer's claim this node has not projected yet may have been reopened for real,
    /// so it is left to the claims. Returns the settled runs.
    pub fn settle_runs_for_canonical_replay(&self) -> Result<Vec<String>> {
        let mut connection = self.connection.write();
        let transaction = connection.transaction()?;
        if transaction
            .query_row(
                "SELECT 1 FROM meta WHERE key='canonical_replay_settled'",
                [],
                |_| Ok(()),
            )
            .optional()?
            .is_some()
        {
            return Ok(Vec::new());
        }
        let frontier: u64 = transaction
            .query_row(
                "SELECT last_good_store_index FROM projection_health WHERE aggregate='graph'",
                [],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(0);
        let ended = {
            let mut statement = transaction.prepare(
                "SELECT runs.id, runs.status FROM mission_runs AS runs
                 WHERE (runs.phase='terminal' OR runs.status IN ('completed','failed','cancelled'))
                   AND EXISTS (SELECT 1 FROM claims WHERE claims.subject='mission-run/' || runs.id
                               AND claims.kind='mission-run.created' AND claims.origin=?1)
                   AND NOT EXISTS (
                     SELECT 1 FROM claims
                     WHERE claims.store_index>?2 AND claims.origin<>?1
                       AND (claims.subject='mission-run/' || runs.id
                            OR claims.subject IN (SELECT 'run-generation/' || generations.id
                                                  FROM run_generations AS generations
                                                  WHERE generations.run_id=runs.id)
                            OR (claims.kind='run-generation.created'
                                AND json_extract(claims.body, '$.fields.run')='mission-run/' || runs.id)))
                 ORDER BY runs.id",
            )?;
            statement
                .query_map(params![self.origin, frontier], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?
        };
        replay_graph_from_nothing_tx(&transaction)?;
        let mut settled = Vec::new();
        for (run, status) in ended {
            let reopened = transaction
                .query_row(
                    "SELECT phase<>'terminal' AND status NOT IN ('completed','failed','cancelled')
                     FROM mission_runs WHERE id=?1",
                    [&run],
                    |row| row.get::<_, bool>(0),
                )
                .optional()?
                .unwrap_or(false);
            if reopened {
                let status = if is_terminal_run_state(&status) {
                    status.as_str()
                } else {
                    "cancelled"
                };
                set_mission_run_state_tx(
                    &transaction,
                    &self.origin,
                    &run,
                    status,
                    "terminal",
                    Some("this node treated the run as over before replays followed its claims"),
                )?;
                settled.push(run);
            }
        }
        reapply_local_work_lease_renewals_tx(&transaction)?;
        transaction.execute(
            "INSERT INTO projection_health(aggregate, status, last_good_store_index, updated_at_unix_ms)
             VALUES ('graph', 'healthy', ?1, ?2)
             ON CONFLICT(aggregate) DO UPDATE SET status='healthy', last_good_store_index=excluded.last_good_store_index,
                error_code=NULL, error_message=NULL, updated_at_unix_ms=excluded.updated_at_unix_ms",
            params![current_index_tx(&transaction)?, now_ms().to_string()],
        )?;
        transaction.execute(
            "INSERT OR REPLACE INTO meta(key, value) VALUES ('canonical_replay_settled', ?1)",
            [now_ms().to_string()],
        )?;
        transaction.commit()?;
        Ok(settled)
    }
}

type StepStateRow = (String, bool, u32, String, String, String, String, String);

const AGENT_WORK_PREVIEW_LIMIT: usize = 5;

/// The most one `work extend` adds to a step attempt's execution budget.
const MAX_STEP_EXTENSION_MS: u64 = 7 * 24 * 60 * 60 * 1000;

static RUNTIME_ID_SUBJECTS: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| format!(
    "SELECT subject FROM claims INDEXED BY claims_runtime_id_subject_index
     WHERE {ACTUAL_STATE_CLAIM} AND typeof({RUNTIME_ID_FIELD})='text'
       AND {RUNTIME_ID_FIELD}=?1 AND store_index<=?2
       AND EXISTS (SELECT 1 FROM claims observed INDEXED BY claims_runtime_subject_index
                   WHERE observed.kind='runtime.observed' AND observed.subject=claims.subject
                     AND observed.store_index<=?2)
     UNION
     SELECT subject FROM claims INDEXED BY claims_runtime_subject_index
     WHERE kind='runtime.observed' AND subject=?3 AND store_index<=?2
     ORDER BY subject"
));

/// Runtime authority without presentation, harness, planning or claim-history reduction.
pub struct RuntimeAuthority {
    pub actual: Option<Value>,
    pub actual_origin: Option<String>,
    pub reachability: String,
}

/// A run's latest state claim: when it was accepted, the status it set, and the outcome
/// someone set when it moved a finished run to another finished state.
#[derive(Clone, Debug, Default)]
pub struct MissionRunStateMoment {
    pub since_unix_ms: u128,
    pub status: Option<String>,
    pub outcome: Option<MissionRunOutcomeView>,
}

/// The most changed items one resource-change message lists.
const MESSAGE_ITEMS: usize = 20;

/// Subscription subjects change when a watch mission is restarted. Look up the item and exact
/// head in the graph's existing request claims, including claims written before delivery keys
/// were added, so a replacement watch cannot review an old head again.
fn collection_delivery_was_requested_tx(
    transaction: &rusqlite::Transaction<'_>,
    mission: &str,
    delivery_scope: &str,
    resource: &str,
    head: Option<&str>,
    delivery_key: &str,
) -> rusqlite::Result<bool> {
    let keyed = transaction
        .query_row(
            "SELECT 1 FROM claims WHERE kind='subscription.mission-requested'
         AND json_extract(body, '$.fields.delivery_key')=?1 LIMIT 1",
            [delivery_key],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if keyed {
        return Ok(true);
    }
    let mut statement = transaction.prepare(
        "SELECT subject, body FROM claims WHERE kind='subscription.mission-requested'
         AND json_extract(body, '$.fields.mission')=?1
         AND json_extract(body, '$.fields.resource')=?2
         AND json_extract(body, '$.fields.delivery_key') IS NULL",
    )?;
    let requests = statement
        .query_map(params![format!("mission/{mission}"), resource], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (subscription, body) in requests {
        if subscription.rsplit('/').next() != Some(delivery_scope) {
            continue;
        }
        let body: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
        let Some(discovery) = body.pointer("/fields/discovery").and_then(Value::as_str) else {
            continue;
        };
        let observed = transaction
            .query_row("SELECT body FROM claims WHERE id=?1", [discovery], |row| {
                row.get::<_, String>(0)
            })
            .optional()?;
        let observed = observed.and_then(|body| serde_json::from_str::<Value>(&body).ok());
        let old_head = match observed
            .as_ref()
            .and_then(|value| value.pointer("/fields/facts/head_sha"))
            .and_then(Value::as_str)
        {
            Some(old_head) => Some(old_head.to_owned()),
            None => listed_head_tx(transaction, observed.as_ref())?,
        };
        if old_head.as_deref() == head {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The seat that registered a comment or review, by its GitHub ID, never by login.
pub fn github_posted_by_tx(
    connection: &Connection,
    thread: &crate::github_watch::ThreadRef,
    entry: &Value,
) -> Result<Option<String>, St3Error> {
    let (kind, id) = crate::resource::recent_comment_key(entry);
    github_post_agent_tx(connection, &thread.locator(), &kind, id)
}

/// The new items and mentions one observation adds to a batched delivery, each with its delivery
/// key and the item claim it came from: every new pull request head, new issue, and new mention of
/// a named login, on items no live agent owns. An owned item goes to its owner as one message
/// instead, and an item an earlier observation already sent there is skipped.
#[allow(clippy::too_many_arguments)]
fn batch_entries_tx(
    transaction: &rusqlite::Transaction<'_>,
    origin: &str,
    batch_id: &str,
    subscription_subject: &str,
    subscription: &crate::model::SubscriptionSpec,
    mission: &str,
    repository_identity: &Value,
    selected: &[String],
    collection_discoveries: &BTreeMap<String, Vec<(String, String)>>,
    item_claims: &[(BTreeSet<String>, String, String, Value)],
    item_mentions: &[(String, String, Value, Vec<Value>)],
    message_subjects: &mut Vec<String>,
) -> Result<Vec<(String, String, Value)>, St3Error> {
    let delivery_scope = subscription_subject
        .rsplit('/')
        .next()
        .expect("a subscription subject has a local name");
    let mut unowned = Vec::new();
    for field in selected
        .iter()
        .filter(|field| matches!(field.as_str(), "pull_requests" | "issues"))
    {
        for (item_subject, claim) in collection_discoveries.get(field).into_iter().flatten() {
            let Some((_, _, _, item_facts)) = item_claims
                .iter()
                .find(|(_, subject, ..)| subject == item_subject)
            else {
                continue;
            };
            let kind = if field == "pull_requests" {
                "pull_request"
            } else {
                "issue"
            };
            let number = item_facts.get("number").and_then(Value::as_u64);
            let head = item_facts.get("head_sha").and_then(Value::as_str);
            let delivery_key = canonical_hash(&(
                mission,
                delivery_scope,
                repository_identity,
                kind,
                number,
                head,
            ))
            .map_err(internal)?;
            if routed_to_owner_tx(transaction, &delivery_key)? {
                continue;
            }
            if subscription.owner_message
                && let Some((owner, why)) =
                    item_owner_tx(transaction, item_facts).map_err(internal)?
            {
                let (title, detail) = item_route_text(item_facts);
                message_subjects.extend(route_item_to_owner_tx(
                    transaction,
                    origin,
                    batch_id,
                    subscription_subject,
                    item_subject,
                    &owner,
                    &why,
                    &title,
                    &detail,
                    &delivery_key,
                    std::slice::from_ref(claim),
                )?);
                continue;
            }
            let mut entry = json!({"kind": kind, "resource": item_subject, "claim": claim});
            for name in [
                "number",
                "title",
                "url",
                "author",
                "state",
                "draft",
                "head_sha",
                "branch",
                "opened_by",
                "opened_by_run",
            ] {
                if let Some(value) = item_facts.get(name) {
                    entry[name] = value.clone();
                }
            }
            unowned.push((delivery_key, claim.clone(), entry));
        }
    }
    for (item_subject, claim, item_facts, mentions) in item_mentions {
        for mention in mentions {
            let text = |name: &str| {
                mention
                    .get(name)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
            };
            let login = text("login");
            // A login that mentions itself tells its person nothing.
            if text("by").eq_ignore_ascii_case(login)
                || !subscription
                    .mentions
                    .iter()
                    .any(|known| known.eq_ignore_ascii_case(login))
            {
                continue;
            }
            let number = item_facts.get("number").and_then(Value::as_u64);
            let item_title = item_facts
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let delivery_key = canonical_hash(&(
                "mention",
                delivery_scope,
                repository_identity,
                number,
                login.to_ascii_lowercase(),
                text("url"),
                text("at"),
            ))
            .map_err(internal)?;
            if routed_to_owner_tx(transaction, &delivery_key)? {
                continue;
            }
            if subscription.owner_message
                && let Some((owner, why)) =
                    item_owner_tx(transaction, item_facts).map_err(internal)?
            {
                let by = Some(text("by"))
                    .filter(|by| !by.is_empty())
                    .unwrap_or("someone");
                message_subjects.extend(route_item_to_owner_tx(
                    transaction,
                    origin,
                    batch_id,
                    subscription_subject,
                    item_subject,
                    &owner,
                    &why,
                    &format!(
                        "{by} mentioned @{login} on #{}: {item_title}",
                        number.unwrap_or_default()
                    ),
                    &format!(
                        "{}\n\n{item_title}: {}",
                        text("url"),
                        item_facts
                            .get("url")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                    ),
                    &delivery_key,
                    std::slice::from_ref(claim),
                )?);
                continue;
            }
            unowned.push((
                delivery_key,
                claim.clone(),
                json!({
                    "kind": "mention",
                    "resource": item_subject,
                    "claim": claim,
                    "number": number,
                    "title": item_title,
                    "item_url": item_facts.get("url"),
                    "login": login,
                    "by": text("by"),
                    "url": text("url"),
                    "at": text("at"),
                }),
            ));
        }
    }
    unowned.truncate(ENTRIES_PER_REQUEST);
    for (key, _, entry) in &mut unowned {
        let kind = entry["kind"].as_str().unwrap_or_default().to_owned();
        entry["why"] = Value::String(match kind.as_str() {
            "pull_request" => "a new pull request head that no live agent owns".into(),
            "issue" => "a new issue that no live agent owns".into(),
            _ => format!(
                "{} mentioned @{}",
                entry["by"].as_str().unwrap_or("someone"),
                entry["login"].as_str().unwrap_or_default()
            ),
        });
        if let Some(repository) = entry["resource"]
            .as_str()
            .and_then(|item| item.rsplitn(3, '/').nth(2))
        {
            entry["repository"] = Value::String(repository.to_owned());
        }
        entry["marker"] = Value::String(key.clone());
    }
    Ok(unowned)
}

/// The mentions of an item that are new with this observation: those a recorded item did not
/// know, made no earlier than five minutes before the item's last observation. Turning mentions
/// on, an edit to an old comment, or a body read again never reports an old mention. A new item's
/// own mentions are not new: the item itself reaches its review or triage, and an intake turned
/// back on finds items opened while it was off, whose body mentions are old news.
fn new_mentions(prior: Option<&(Value, u128)>, facts: &Value) -> Vec<Value> {
    let mentions = facts
        .get("mentions")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let Some((prior, observed_at)) = prior else {
        return Vec::new();
    };
    let known = prior
        .get("mentions")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    mentions
        .into_iter()
        .filter(|mention| !known.contains(mention))
        .filter(|mention| {
            mention
                .get("at")
                .and_then(Value::as_str)
                .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
                .is_some_and(|at| {
                    (at.timestamp_millis().max(0) as u128).saturating_add(5 * 60_000)
                        >= *observed_at
                })
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn prepare_mission_run_declaration(
    connection: &Connection,
    declaration: &MissionRunDeclaration,
    store_index: u64,
    tokens: &mut BTreeMap<String, Vec<String>>,
    actions: &mut Vec<PlannedAction>,
    blockers: &mut Vec<String>,
    warnings: &mut Vec<String>,
) -> Result<(), St3Error> {
    let run_id = declaration
        .subject
        .strip_prefix("mission-run/")
        .unwrap_or(&declaration.subject);
    tokens.insert(
        declaration.subject.clone(),
        claim_ids_at(connection, &declaration.subject, Some(store_index))
            .map_err(internal)?
            .into_iter()
            .last()
            .into_iter()
            .collect(),
    );
    let current = connection
        .query_row(
            "SELECT mission_id, initial_revision, workspace, requester, status, current_generation_id,
                    inputs, mode
             FROM mission_runs WHERE id=?1",
            [run_id],
            |row| {
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
            },
        )
        .optional()
        .map_err(internal)?;
    if let Some(creation) = &declaration.creation {
        if let Some((mission, revision, workspace, requester, _, _, inputs, mode)) = &current {
            let stored_inputs = serde_json::from_str::<BTreeMap<String, MissionRunInput>>(inputs)
                .map_err(internal)?;
            let stored_values = stored_inputs
                .into_iter()
                .map(|(name, input)| (name, input.value))
                .collect::<BTreeMap<_, _>>();
            if mission != &creation.mission
                || revision != &creation.revision
                || workspace != &creation.workspace
                || requester != &creation.requester
                || stored_values != creation.inputs
                || mode != &creation.mode
                || mission_run_after_tx(connection, run_id)? != creation.after
                || mission_run_report_tx(connection, run_id)?
                    != requested_run_report(connection, creation)?
            {
                blockers.push(format!(
                    "mission run `{}` already exists with different creation fields",
                    declaration.subject
                ));
            }
        } else {
            let mission = connection
                .query_row(
                    "SELECT body FROM mission_revisions WHERE mission_id=?1 AND revision=?2",
                    params![creation.mission, creation.revision],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(internal)?
                .map(|body| serde_json::from_str::<MissionSpec>(&body))
                .transpose()
                .map_err(internal)?;
            match mission {
                None => blockers.push(missing_mission_revision(
                    connection,
                    &creation.mission,
                    &creation.revision,
                )?),
                Some(mission) if mission.state != MissionState::Ready => blockers.push(format!(
                    "mission `mission/{}` revision `{}` is not ready",
                    creation.mission, creation.revision
                )),
                Some(mission) => {
                    if let Err(error) =
                        resolve_mission_run_inputs(connection, &mission, &creation.inputs)
                    {
                        blockers.push(error.message);
                    }
                    if let Err(error) = enforce_mission_run_capacity(connection, &mission) {
                        blockers.push(error.message);
                    }
                    if let Some(after) = &creation.after
                        && let Err(error) =
                            ensure_awaited_mission_run_tx(connection, &declaration.subject, after)
                                .and_then(|()| crate::mission::mission_after_run(mission, after))
                    {
                        blockers.push(error.message);
                    }
                    actions.push(PlannedAction {
                        subject: declaration.subject.clone(),
                        action: "start-mission-run".into(),
                        reason: "the named mission run does not exist".into(),
                    });
                }
            }
        }
    }
    if current.is_none() && declaration.creation.is_none() {
        blockers.push(format!(
            "mission run `{}` does not exist",
            declaration.subject
        ));
        return Ok(());
    }
    let status = current.as_ref().map(|value| value.4.as_str());
    let current_generation = current.as_ref().map(|value| value.5.as_str());
    for revision in declaration.revisions.values() {
        if current_generation.is_some_and(|current| {
            revision
                .from_generation
                .strip_prefix("run-generation/")
                .unwrap_or(&revision.from_generation)
                != current
        }) {
            blockers.push(format!(
                "revision `{}` names a stale run generation",
                revision.id
            ));
        }
        let exists = connection
            .query_row(
                "SELECT 1 FROM mission_revisions WHERE mission_id=?1 AND revision=?2",
                params![revision.mission, revision.revision],
                |_| Ok(()),
            )
            .optional()
            .map_err(internal)?
            .is_some();
        if !exists {
            blockers.push(format!(
                "revision `{}` names a mission that is not stored: {}",
                revision.id,
                missing_mission_revision(connection, &revision.mission, &revision.revision)?
            ));
        }
        match publication_operation_is_new(
            connection,
            &declaration.subject,
            "revision",
            &revision.id,
            revision,
        ) {
            Ok(true) => actions.push(PlannedAction {
                subject: declaration.subject.clone(),
                action: format!("revise:{}", revision.id),
                reason: revision.reason.clone(),
            }),
            Ok(false) => {}
            Err(error) => blockers.push(error.message),
        }
    }
    for reset in declaration.resets.values() {
        if current_generation.is_some_and(|current| {
            reset
                .from_generation
                .strip_prefix("run-generation/")
                .unwrap_or(&reset.from_generation)
                != current
        }) {
            blockers.push(format!("reset `{}` names a stale run generation", reset.id));
        }
        match publication_operation_is_new(
            connection,
            &declaration.subject,
            "reset",
            &reset.id,
            reset,
        ) {
            Ok(true) => actions.push(PlannedAction {
                subject: runtime_subject_for_run(run_id, &reset.runtime),
                action: format!("reset:{}", reset.id),
                reason: reset.reason.clone(),
            }),
            Ok(false) => {}
            Err(error) => blockers.push(error.message),
        }
    }
    for cancellation in declaration.cancellations.values() {
        let is_new = match publication_operation_is_new(
            connection,
            &declaration.subject,
            "cancellation",
            &cancellation.id,
            cancellation,
        ) {
            Ok(value) => value,
            Err(error) => {
                blockers.push(error.message);
                continue;
            }
        };
        if !is_new {
            continue;
        } else if matches!(status, Some("completed" | "failed" | "cancelled")) {
            warnings.push(format!(
                "mission run `{}` is already {}",
                declaration.subject,
                status.unwrap_or_default()
            ));
        } else {
            actions.push(PlannedAction {
                subject: declaration.subject.clone(),
                action: format!("cancel:{}", cancellation.id),
                reason: cancellation.reason.clone(),
            });
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn prepare_planning_session_declaration(
    connection: &Connection,
    declaration: &PlanningSessionDeclaration,
    store_index: u64,
    tokens: &mut BTreeMap<String, Vec<String>>,
    actions: &mut Vec<PlannedAction>,
    blockers: &mut Vec<String>,
) -> Result<(), St3Error> {
    let id = declaration
        .subject
        .strip_prefix("planning-session/")
        .unwrap_or(&declaration.subject);
    tokens.insert(
        declaration.subject.clone(),
        claim_ids_at(connection, &declaration.subject, Some(store_index))
            .map_err(internal)?
            .into_iter()
            .last()
            .into_iter()
            .collect(),
    );
    let current = connection
        .query_row(
            "SELECT mission_id, request_ref, workspace, requester, planner, status,
                    target_run_id, source_generation_id
             FROM planning_sessions WHERE id=?1",
            [id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, Option<String>>(7)?,
                ))
            },
        )
        .optional()
        .map_err(internal)?;
    if let Some(creation) = &declaration.creation {
        if let Some((mission, request, workspace, requester, planner, _, target, generation)) =
            &current
        {
            if mission != &creation.mission
                || request != &creation.request
                || workspace != &creation.workspace
                || requester != &creation.requester
                || planner != &crate::graph::planning_planner_subject(&declaration.subject)
                || target.as_deref()
                    != creation
                        .target_run
                        .as_deref()
                        .map(|value| value.strip_prefix("mission-run/").unwrap_or(value))
                || generation.as_deref()
                    != creation
                        .target_generation
                        .as_deref()
                        .map(|value| value.strip_prefix("run-generation/").unwrap_or(value))
            {
                blockers.push(format!(
                    "launch `{}` already exists with different creation fields",
                    declaration.subject
                ));
            }
        } else {
            if let (Some(run), Some(generation)) =
                (&creation.target_run, &creation.target_generation)
            {
                let run_id = run.strip_prefix("mission-run/").unwrap_or(run);
                let expected_generation = generation
                    .strip_prefix("run-generation/")
                    .unwrap_or(generation);
                let actual = connection
                    .query_row(
                        "SELECT current_generation_id FROM mission_runs WHERE id=?1",
                        [run_id],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()
                    .map_err(internal)?;
                if actual.as_deref() != Some(expected_generation) {
                    blockers.push(format!(
                        "launch `{}` names a stale target generation",
                        declaration.subject
                    ));
                }
            }
            actions.push(PlannedAction {
                subject: declaration.subject.clone(),
                action: "start-launch".into(),
                reason: "the named launch does not exist".into(),
            });
        }
    }
    if current.is_none() && declaration.creation.is_none() {
        blockers.push(format!("launch `{}` does not exist", declaration.subject));
    }
    for feedback in declaration.feedback.values() {
        match publication_operation_is_new(
            connection,
            &declaration.subject,
            "feedback",
            &feedback.id,
            feedback,
        ) {
            Ok(true) => actions.push(PlannedAction {
                subject: declaration.subject.clone(),
                action: format!("feedback:{}", feedback.id),
                reason: "the publication supplies launch feedback".into(),
            }),
            Ok(false) => {}
            Err(error) => blockers.push(error.message),
        }
    }
    for cancellation in declaration.cancellations.values() {
        match publication_operation_is_new(
            connection,
            &declaration.subject,
            "cancellation",
            &cancellation.id,
            cancellation,
        ) {
            Ok(true) => actions.push(PlannedAction {
                subject: declaration.subject.clone(),
                action: format!("cancel:{}", cancellation.id),
                reason: cancellation.reason.clone(),
            }),
            Ok(false) => {}
            Err(error) => blockers.push(error.message),
        }
    }
    Ok(())
}

/// Who a run reports to now: its latest report claim, else its creation claim.
fn current_run_report_tx(
    transaction: &Transaction<'_>,
    run: &str,
) -> Result<Option<(String, u64, bool, bool)>, St3Error> {
    let claim = match latest_claim_of_kind_tx(transaction, run, RUN_REPORT_KIND)? {
        Some(claim) => Some(claim),
        None => latest_claim_of_kind_tx(transaction, run, "mission-run.created")?,
    };
    Ok(claim.and_then(|claim| run_report_fields(&claim.body)))
}

fn renew_nested_ancestor_leases_tx(
    transaction: &Transaction<'_>,
    generation: &str,
    step_path: &str,
    actor: &str,
    incarnation: Option<&str>,
    now: u128,
) -> std::result::Result<(), St3Error> {
    let generation = generation_id_from_subject(generation);
    let expiry = now.saturating_add(600_000);
    transaction
        .execute(
            "UPDATE step_runs
             SET lease_expires_at_unix_ms=?1, updated_at_unix_ms=?2
             WHERE generation_id=?3
               AND substr(?4, 1, length(step_path) + 1)=step_path || '/'
               AND status IN ('claimed','working','verifying')
               AND lease_owner=?5
               AND lease_incarnation IS ?6",
            params![
                expiry.to_string(),
                now.to_string(),
                generation,
                step_path,
                actor,
                incarnation
            ],
        )
        .map_err(internal)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn append_first_receipt_claim_tx(
    transaction: &Transaction<'_>,
    origin: &str,
    subject: &str,
    kind: &str,
    actor: Option<&str>,
    body: &Value,
    predecessors: &[String],
    forced_batch: Option<&str>,
    key: &mut Option<&str>,
) -> Result<ClaimRecord> {
    if let Some(key) = key.take() {
        append_receipt_claim_tx(
            transaction,
            origin,
            subject,
            kind,
            actor,
            body,
            predecessors,
            forced_batch,
            key,
        )
    } else {
        append_claim_tx(
            transaction,
            origin,
            subject,
            kind,
            actor,
            body,
            predecessors,
            forced_batch,
        )
    }
}

fn retry_step_tx(
    transaction: &Transaction<'_>,
    origin: &str,
    subject: &str,
    actor: Option<&str>,
    reason: &str,
    backoff_ms: u64,
    receipt_key: Option<&str>,
) -> Result<bool> {
    let current: Option<StepRetryRow> = transaction
        .query_row(
            "SELECT step_runs.status, step_runs.attempt,
                    step_runs.generation_id=mission_runs.current_generation_id,
                    mission_runs.status, mission_runs.phase, run_generations.status,
                    root_runs.status, root_runs.phase
             FROM step_runs JOIN mission_runs ON mission_runs.id=step_runs.run_id
             JOIN mission_runs root_runs ON root_runs.id=mission_runs.root_run_id
             JOIN run_generations ON run_generations.id=step_runs.generation_id
             WHERE step_runs.subject=?1",
            [&subject],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                ))
            },
        )
        .optional()?;
    let Some((
        status,
        attempt,
        is_current,
        run_status,
        run_phase,
        generation_status,
        root_status,
        root_phase,
    )) = current
    else {
        return Ok(false);
    };
    if !is_current
        || status != "failed"
        || is_terminal_run_state(&run_status)
        || run_phase == "terminal"
        || is_terminal_generation_state(&generation_status)
        || is_terminal_run_state(&root_status)
        || root_phase == "terminal"
    {
        return Ok(false);
    }
    let now = now_ms();
    let not_before = now.saturating_add(backoff_ms as u128);
    transaction.execute(
        "UPDATE step_runs SET status='pending', attempt=?2, worker_reported=0, lease_owner=NULL,
                lease_incarnation=NULL, lease_expires_at_unix_ms=NULL, blocked_reason=?3,
                not_before_unix_ms=?4, activated_at_unix_ms=NULL, updated_at_unix_ms=?5 WHERE subject=?1",
        params![subject, attempt.saturating_add(1), reason, not_before.to_string(), now.to_string()])?;
    let mut body = json!({"fields": {"status": "pending", "attempt": attempt.saturating_add(1), "reason": reason, "not_before_unix_ms": not_before}});
    if let Some(key) = receipt_key {
        smallclaims::store::idempotency::attach(&mut body, key)?;
    }
    let record = append_claim_tx(
        transaction,
        origin,
        subject,
        "step-run.retried",
        actor,
        &body,
        &[],
        None,
    )?;
    if receipt_key.is_some() {
        register_operation_tx(transaction, &record)?;
    }
    Ok(true)
}

fn intent_leaves_at(
    connection: &Connection,
    subject: &str,
    at_index: Option<u64>,
) -> Result<Vec<String>> {
    let through = at_index.unwrap_or(i64::MAX as u64);
    let mut statement = connection.prepare_cached(
        "SELECT id, predecessors FROM claims
         WHERE subject=?1 AND kind='intent.desired' AND store_index<=?2
           AND NOT EXISTS (
               SELECT 1 FROM replica_records
               WHERE replica_records.claim_id=claims.id
                 AND replica_records.state='repaired'
           )
         ORDER BY id",
    )?;
    let rows = statement
        .query_map(params![subject, through], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let referenced = rows
        .iter()
        .flat_map(|(_, predecessors)| {
            serde_json::from_str::<Vec<String>>(predecessors).unwrap_or_default()
        })
        .collect::<BTreeSet<_>>();
    Ok(rows
        .into_iter()
        .map(|(id, _)| id)
        .filter(|id| !referenced.contains(id))
        .collect())
}

fn subscription_condition_matches(condition: &SubscriptionConditionSpec, facts: &Value) -> bool {
    match condition {
        SubscriptionConditionSpec::Field {
            path,
            operator,
            value,
        } => value_at_path(facts, path)
            .is_some_and(|found| predicate_value_matches(found, operator, value)),
        SubscriptionConditionSpec::Every { path, fields }
        | SubscriptionConditionSpec::NotEvery { path, fields } => {
            let Some(items) = value_at_path(facts, path).and_then(Value::as_array) else {
                return false;
            };
            let every = items.iter().all(|item| {
                fields.iter().all(|field| {
                    value_at_path(item, &field.path).is_some_and(|found| {
                        predicate_value_matches(found, &field.operator, &field.value)
                    })
                })
            });
            matches!(condition, SubscriptionConditionSpec::Every { .. }) == every
        }
    }
}

fn set_mission_run_state_tx(
    transaction: &Transaction<'_>,
    origin: &str,
    run: &str,
    status: &str,
    phase: &str,
    reason: Option<&str>,
) -> Result<bool> {
    let run = run.strip_prefix("mission-run/").unwrap_or(run);
    let subject = format!("mission-run/{run}");
    let current: Option<(String, String)> = transaction
        .query_row(
            "SELECT status, phase FROM mission_runs WHERE id=?1",
            [run],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((current_status, current_phase)) = current else {
        return Ok(false);
    };
    let target_is_terminal = is_terminal_run_state(status) || phase == "terminal";
    let now = now_ms();
    if current_phase == "terminal"
        || (current_status == status && current_phase == phase && target_is_terminal)
    {
        let generation: String = transaction.query_row(
            "SELECT current_generation_id FROM mission_runs WHERE id=?1",
            [run],
            |row| row.get(0),
        )?;
        let reason = reason.unwrap_or("the owning mission run is terminal");
        let mut claim_ids = cancel_descendant_mission_runs_tx(
            transaction,
            origin,
            &generation,
            "daemon/runtime",
            reason,
            now,
        )?;
        claim_ids.extend(terminalize_run_steps_tx(
            transaction,
            origin,
            run,
            reason,
            None,
            None,
            now,
        )?);
        return Ok(!claim_ids.is_empty());
    }
    if current_status == status && current_phase == phase {
        return Ok(false);
    }
    if (current_phase.starts_with("cleanup-") && phase != "terminal")
        || (current_phase == "final-cancelled"
            && !matches!(phase, "final-cancelled" | "cleanup-cancelled" | "terminal"))
    {
        return Ok(false);
    }
    transaction.execute(
        "UPDATE mission_runs SET status=?2, phase=?3, updated_at_unix_ms=?4 WHERE id=?1",
        params![run, status, phase, now.to_string()],
    )?;
    transaction.execute(
        "UPDATE run_generations SET status=?2, updated_at_unix_ms=?3
         WHERE id=(SELECT current_generation_id FROM mission_runs WHERE id=?1)",
        params![run, status, now.to_string()],
    )?;
    let body = json!({"fields": {"status": status, "phase": phase, "reason": reason}});
    append_claim_tx(
        transaction,
        origin,
        &subject,
        "mission-run.state",
        None,
        &body,
        &[],
        None,
    )?;
    let generation: String = transaction.query_row(
        "SELECT current_generation_id FROM mission_runs WHERE id=?1",
        [run],
        |row| row.get(0),
    )?;
    append_claim_tx(
        transaction,
        origin,
        &format!("run-generation/{generation}"),
        "run-generation.state",
        None,
        &body,
        &[],
        None,
    )?;
    if phase.starts_with("cleanup-") {
        terminalize_run_steps_tx(
            transaction,
            origin,
            run,
            reason.unwrap_or("the mission run entered cleanup"),
            Some("daemon/runtime"),
            None,
            now,
        )?;
    }
    if target_is_terminal {
        cancel_descendant_mission_runs_tx(
            transaction,
            origin,
            &generation,
            "daemon/runtime",
            reason.unwrap_or("the owning mission run is terminal"),
            now,
        )?;
        terminalize_run_steps_tx(
            transaction,
            origin,
            run,
            reason.unwrap_or("the owning mission run is terminal"),
            None,
            None,
            now,
        )?;
    }
    Ok(true)
}

/// The seat's next work when claiming `subject` would take a later run first.
///
/// Only the order of runs is enforced. Inside one run the mission's
/// dependencies decide, and a nested step reached through its parent is not a
/// separate queue entry, so neither is refused here.
fn seat_order_conflict_tx(
    connection: &Connection,
    agent: &str,
    subject: &str,
) -> Result<Option<String>> {
    let order = seat_run_orders_tx(connection, Some(agent))?
        .remove(agent)
        .unwrap_or_default();
    let rows = seat_step_rows_tx(connection, Some(agent))?;
    let steps = rows
        .iter()
        .map(RosterStepRow::seat_step)
        .collect::<Vec<_>>();
    let selection = seat_queue::select(agent, &steps, &order);
    let Some(next) = selection.next() else {
        return Ok(None);
    };
    if next == subject || !selection.ready.contains(&subject) {
        return Ok(None);
    }
    let run = |candidate: &str| {
        steps
            .iter()
            .find(|step| step.subject == candidate)
            .map(|step| step.run)
    };
    Ok((run(next) != run(subject)).then(|| next.to_owned()))
}

fn append_review_remediation_warnings(mission: &MissionSpec, warnings: &mut Vec<String>) {
    fn describes(step: &crate::model::StepSpec, terms: &[&str]) -> bool {
        let description = std::iter::once(step.id.as_str())
            .chain(step.title.as_deref())
            .chain(step.goals.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" ")
            .to_ascii_lowercase();
        terms.iter().any(|term| description.contains(term))
    }

    for id in &mission.display_order {
        let step = &mission.steps[id];
        let remediation = describes(step, &["fix", "remediat", "repair", "resolve finding"]);
        if remediation {
            for dependency in &step.dependencies {
                let DependencySpec::Step {
                    step: target,
                    state,
                } = dependency
                else {
                    continue;
                };
                let Some(predecessor) = mission.steps.get(target) else {
                    continue;
                };
                if state == "completed"
                    && describes(predecessor, &["review", "validation", "audit"])
                    && !describes(predecessor, &["report-only"])
                {
                    warnings.push(format!(
                        "remediation step `{}` depends on review step `{}` completing; if findings make the review fail, failure propagation makes remediation unreachable. Make the review report findings and complete, or use an explicit bounded review/fix loop",
                        step.path, predecessor.path
                    ));
                }
            }
        }
        if let Some(nested) = &step.nested_mission {
            append_review_remediation_warnings(nested, warnings);
        }
    }
}

/// Return the failed steps of a failed root run when they are the only reason it failed, or
/// explain why the run cannot reopen.
fn failed_run_reopen_blocker(run: &MissionRunView) -> Result<Vec<&str>, String> {
    if run.parent_step_run.is_some() {
        return Err("a child run reopens only through its parent step".into());
    }
    if run.mode == "eval" {
        return Err("an eval run keeps its verdict".into());
    }
    if run
        .deadline_at_unix_ms
        .is_some_and(|deadline| deadline <= now_ms())
    {
        return Err("its mission timeout has expired".into());
    }
    if let Some(step) = run.steps.iter().find(|step| {
        step.status == "cancelled"
            && !step
                .blocked_reason
                .as_deref()
                .is_some_and(|reason| FAILURE_CANCELLATION_REASONS.contains(&reason))
    }) {
        return Err(format!(
            "step `{}` was cancelled: {}",
            step.step,
            step.blocked_reason
                .as_deref()
                .unwrap_or("no reason recorded")
        ));
    }
    let failed = run
        .steps
        .iter()
        .filter(|step| step.status == "failed")
        .map(|step| step.step.as_str())
        .collect::<Vec<_>>();
    if failed.is_empty() {
        return Err("no step failed, so the run failed for another reason".into());
    }
    Ok(failed)
}

fn not_reopenable(run: &str, reason: &str) -> St3Error {
    St3Error::new(
        "mission-run-not-reopenable",
        format!("mission run `{run}` cannot reopen: {reason}"),
    )
}

/// Return the current-generation successor of a superseded step when it carries this actor's claim.
fn carried_claim_successor_tx(
    connection: &Connection,
    step: &StepRunView,
    actor: &str,
) -> rusqlite::Result<Option<StepRunView>> {
    let run_id = step.run.strip_prefix("mission-run/").unwrap_or(&step.run);
    connection
        .query_row(
            "SELECT subject, run_id, step_path, definition_hash, status, attempt, assignee, available_to, agentless, title, goals, worker_reported,
                    lease_owner, lease_incarnation, lease_expires_at_unix_ms, blocked_reason, not_before_unix_ms, created_at_unix_ms, updated_at_unix_ms, readiness_epoch, constraints
             FROM step_runs
             WHERE run_id=?1 AND step_path=?2 AND lease_owner=?3 AND generation_id<>?4
               AND generation_id=(SELECT current_generation_id FROM mission_runs WHERE id=?1)",
            params![run_id, step.step, actor, generation_id_from_subject(&step.generation)],
            step_run_from_row,
        )
        .optional()
}

fn mission_output_authority(
    connection: &Connection,
    current: &StepRunView,
    actor: &str,
    incarnation: Option<&str>,
    now: u128,
) -> Result<bool> {
    let lease_matches = |status: &str,
                         owner: Option<&str>,
                         lease_incarnation: Option<&str>,
                         expiry: Option<u128>| {
        matches!(status, "claimed" | "working")
            && owner == Some(actor)
            && expiry.is_some_and(|expiry| expiry > now)
            && lease_incarnation.is_none_or(|lease| Some(lease) == incarnation)
    };
    if lease_matches(
        &current.status,
        current.claimant.as_deref(),
        current.claim_incarnation.as_deref(),
        current.claim_expires_at_unix_ms,
    ) {
        return Ok(true);
    }

    let prefix = format!("{}/", current.step);
    let mut statement = connection.prepare(
        "SELECT status, lease_owner, lease_incarnation, lease_expires_at_unix_ms
         FROM step_runs
         WHERE run_id=?1 AND substr(step_path, 1, length(?2))=?2",
    )?;
    let leases = statement
        .query_map(
            params![
                current
                    .run
                    .strip_prefix("mission-run/")
                    .unwrap_or(&current.run),
                prefix
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            },
        )?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(leases.into_iter().any(|(status, owner, lease, expiry)| {
        lease_matches(
            &status,
            owner.as_deref(),
            lease.as_deref(),
            expiry.and_then(|value| value.parse().ok()),
        )
    }))
}

fn mission_root_tree_run_tx(connection: &Connection, id: &str) -> Result<MissionRunView> {
    // The human tree consumes step state, summaries and queue order, not each worker's wake,
    // timing and harness presentation histories.
    let mut view = mission_run_steps_view_tx(connection, id, true)?;
    enrich_run_step_queues_tx(connection, &mut view)?;
    view.provenance = crate::provenance::read(connection, &view.mission, &view.revision)?;
    view.loops = loop_run_views_tx(connection, &view)?;
    view.outcome = mission_run_outcome_tx(connection, &view)?;
    note_run_view_reads(&view);
    Ok(view)
}

fn run_generation_view_tx(
    connection: &Connection,
    generation_id: &str,
) -> rusqlite::Result<RunGenerationView> {
    let mut view = connection.query_row(
        "SELECT id, run_id, revision, predecessor_id, status, actor, reason,
                created_at_unix_ms, updated_at_unix_ms
         FROM run_generations WHERE id=?1",
        [generation_id],
        |row| {
            let id: String = row.get(0)?;
            let run_id: String = row.get(1)?;
            let predecessor: Option<String> = row.get(3)?;
            let created: String = row.get(7)?;
            let updated: String = row.get(8)?;
            Ok(RunGenerationView {
                subject: format!("run-generation/{id}"),
                id,
                run: format!("mission-run/{run_id}"),
                revision: row.get(2)?,
                predecessor: predecessor.map(|id| format!("run-generation/{id}")),
                status: row.get(4)?,
                actor: row.get(5)?,
                reason: row.get(6)?,
                created_at_unix_ms: created.parse().unwrap_or(0),
                updated_at_unix_ms: updated.parse().unwrap_or(0),
                steps: Vec::new(),
            })
        },
    )?;
    let mut statement = connection.prepare(
        "SELECT subject, run_id, step_path, definition_hash, status, attempt, assignee, available_to, agentless, title, goals, worker_reported,
                lease_owner, lease_incarnation, lease_expires_at_unix_ms, blocked_reason, not_before_unix_ms, created_at_unix_ms, updated_at_unix_ms, readiness_epoch, constraints
         FROM step_runs WHERE generation_id=?1 ORDER BY created_at_unix_ms, step_path",
    )?;
    view.steps = statement
        .query_map([generation_id], step_run_from_row)?
        .collect::<Result<Vec<_>, _>>()?;
    let mut definitions = PinnedDefinitions::default();
    for step in &mut view.steps {
        enrich_step_queue_cached(connection, step, now_ms(), &mut definitions)?;
    }
    Ok(view)
}

fn step_generation_is_current(
    connection: &Connection,
    step: &StepRunView,
) -> rusqlite::Result<bool> {
    let current: String = connection.query_row(
        "SELECT current_generation_id FROM mission_runs WHERE id=?1",
        [step.run.strip_prefix("mission-run/").unwrap_or(&step.run)],
        |row| row.get(0),
    )?;
    Ok(generation_id_from_subject(&step.generation) == current)
}

type StepRetryRow = (String, u32, bool, String, String, String, String, String);

/// The most items and mentions one batched mission request carries.
const ENTRIES_PER_REQUEST: usize = 50;

/// The live agent that owns a repository item, and why: the agent named as its opener while its
/// declaration is live, else the agent of a live mission run that opened it or published its pull
/// request resource. A run's agent is the one working on one of its steps, else its first agent.
fn item_owner_tx(connection: &Connection, facts: &Value) -> Result<Option<(String, String)>> {
    if let Some(agent) = facts.get("opened_by").and_then(Value::as_str)
        && person_work::declaration_live(connection, agent)?
    {
        return Ok(Some((agent.to_owned(), "it opened the item".to_owned())));
    }
    let runs = facts
        .get("opened_by_run")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .into_iter()
        .chain(authoring_pull_request_runs_tx(connection, facts)?)
        .collect::<Vec<_>>();
    for run in runs {
        let run = run.trim_start_matches("mission-run/").to_owned();
        if !person_work::run_live(connection, &run, None, false)? {
            continue;
        }
        let qualified = format!("mission-run/{run}");
        let agents = connection
            .prepare_cached(
                "SELECT subject FROM desired WHERE kind='agent' AND owner_run IN (?1, ?2)
                 ORDER BY subject",
            )?
            .query_map(params![run, qualified], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let working = connection
            .prepare_cached(
                "SELECT lease_owner FROM step_runs WHERE run_id IN (?1, ?2)
                 AND status IN ('claimed', 'working') AND lease_owner IS NOT NULL",
            )?
            .query_map(params![run, qualified], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<BTreeSet<_>>>()?;
        let mut live = Vec::new();
        for agent in agents {
            if person_work::declaration_live(connection, &agent)? {
                live.push(agent);
            }
        }
        if let Some(agent) = live
            .iter()
            .find(|agent| working.contains(*agent))
            .or(live.first())
        {
            return Ok(Some((
                agent.clone(),
                format!("its mission run {qualified} owns the item"),
            )));
        }
    }
    Ok(None)
}

/// Send one message about an item to the agent that owns it. The message's subject comes from
/// the delivery key, so a replacement watch or a repeated observation sends nothing more.
#[allow(clippy::too_many_arguments)]
fn route_item_to_owner_tx(
    transaction: &rusqlite::Transaction<'_>,
    origin: &str,
    batch_id: &str,
    subscription: &str,
    resource: &str,
    owner: &str,
    why: &str,
    title: &str,
    detail: &str,
    delivery_key: &str,
    evidence: &[String],
) -> Result<Option<String>, St3Error> {
    if routed_to_owner_tx(transaction, delivery_key)? {
        return Ok(None);
    }
    let subject = format!("message/route-{}", &delivery_key[..20]);
    append_claim_tx(
        transaction,
        origin,
        &subject,
        "message.sent",
        None,
        &json!({"fields": {
            "from": format!("daemon/{origin}"),
            "to": owner,
            "title": title,
            "content": format!("{detail}\n\nst sent this to you because {why}; no review or person request was made."),
            "status": "sent",
            "tags": ["resource-route", subscription, resource],
        }, "evidence": evidence}),
        &[],
        Some(batch_id),
    )
    .map_err(claim_append_error)?;
    Ok(Some(subject))
}

/// A routed item's message title and its detail: the item, its link and, for a pull request,
/// the head it is at.
fn item_route_text(facts: &Value) -> (String, String) {
    let text = |name: &str| facts.get(name).and_then(Value::as_str).unwrap_or_default();
    let number = facts
        .get("number")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    match facts.get("head_sha").and_then(Value::as_str) {
        Some(head) => (
            format!("Pull request #{number} is at a new head: {}", text("title")),
            format!("{}\nhead {head} on {}", text("url"), text("branch")),
        ),
        None => (
            format!("New issue #{number}: {}", text("title")),
            text("url").to_owned(),
        ),
    }
}

/// Who a new declaration of an existing run asks it to report to, resolved against the mission
/// revision the run started from, so that it compares with what the creation claim stored.
fn requested_run_report(
    connection: &Connection,
    creation: &crate::model::MissionRunCreation,
) -> Result<Option<(String, bool, bool)>, St3Error> {
    let mission = mission_revision_spec_tx(connection, &creation.mission, &creation.revision)?;
    Ok(mission.and_then(|mission| {
        resolved_run_report(
            &mission,
            creation.report_to.as_deref(),
            creation.report_completed,
        )
    }))
}

/// Who a run reports to and whether it also reports completion, as its creation claim recorded
/// them.
fn mission_run_report_tx(
    connection: &Connection,
    run_id: &str,
) -> Result<Option<(String, bool, bool)>, St3Error> {
    connection
        .query_row(
            "SELECT json_extract(body, '$.fields.report_to'),
                    COALESCE(json_extract(body, '$.fields.report_completed'), 0),
                    COALESCE(json_extract(body, '$.fields.report_stalls'), 1)
             FROM claims WHERE subject=?1 AND kind='mission-run.created' LIMIT 1",
            [format!("mission-run/{run_id}")],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, bool>(1)?,
                    row.get::<_, bool>(2)?,
                ))
            },
        )
        .optional()
        .map(|row| row.and_then(|(report_to, completed, stalls)| Some((report_to?, completed, stalls))))
        .map_err(internal)
}

fn mission_run_after_tx(connection: &Connection, run_id: &str) -> Result<Option<String>, St3Error> {
    connection
        .query_row(
            "SELECT after_run FROM mission_run_after WHERE run_id=?1",
            [run_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(internal)
}

/// A run view reads its run, generation and step rows, which change only with claims on those
/// subjects.
fn note_run_view_reads(view: &MissionRunView) {
    smallclaims::touched::note_read(|| view.subject.clone());
    smallclaims::touched::note_read(|| view.generation.clone());
    smallclaims::touched::note_read(|| format!("generations:{}", view.subject));
    for step in &view.steps {
        smallclaims::touched::note_read(|| step.subject.clone());
    }
}

fn value_at_path<'a>(mut value: &'a Value, path: &str) -> Option<&'a Value> {
    for segment in path.split('.') {
        value = value.get(segment)?;
    }
    Some(value)
}

fn predicate_value_matches(found: &Value, operator: &str, expected: &Value) -> bool {
    match operator {
        "is" => found == expected,
        "starts-with" => found
            .as_str()
            .zip(expected.as_str())
            .is_some_and(|(found, expected)| found.starts_with(expected)),
        "contains" => match (found, expected) {
            (Value::String(found), Value::String(expected)) => found.contains(expected),
            (Value::Array(found), expected) => found.contains(expected),
            _ => false,
        },
        _ => false,
    }
}

/// Current held and ready agent steps, for every seat or for one seat.
fn seat_step_rows_tx(connection: &Connection, agent: Option<&str>) -> Result<Vec<RosterStepRow>> {
    let snapshot_unix_ms = now_ms();
    let mut statement = connection.prepare_cached(SEAT_STEP_ROWS)?;
    let mut rows = statement
        .query_map(params![agent, snapshot_unix_ms.to_string()], |row| {
            let mut status: String = row.get(3)?;
            let mut claimant = row.get(5)?;
            let expires: Option<String> = row.get(8)?;
            if matches!(status.as_str(), "claimed" | "working" | "verifying")
                && expires
                    .and_then(|value| value.parse::<u128>().ok())
                    .is_some_and(|expiry| expiry <= snapshot_unix_ms)
            {
                status = "ready".into();
                claimant = None;
            }
            Ok(RosterStepRow {
                subject: row.get(0)?,
                run: format!("mission-run/{}", row.get::<_, String>(1)?),
                step: row.get(2)?,
                status,
                assignee: row.get(4)?,
                claimant,
                available_to: serde_json::from_str(&row.get::<_, String>(6)?).unwrap_or_default(),
                created_at_unix_ms: row.get::<_, String>(7)?.parse().unwrap_or_default(),
                carried_claimant: None,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let carried_claimants = carried_claimants_tx(
        connection,
        rows.iter()
            .filter(|row| row.status == "ready" && row.assignee.is_some())
            .map(|row| row.subject.as_str()),
    )?;
    for row in &mut rows {
        row.carried_claimant = carried_claimants.get(&row.subject).cloned();
    }
    Ok(rows)
}

fn seat_run_orders_tx(
    connection: &Connection,
    agent: Option<&str>,
) -> Result<BTreeMap<String, Vec<String>>> {
    Ok(seat_queue_inputs_tx(connection, agent)?
        .into_iter()
        .map(|(agent, inputs)| {
            let order = inputs.live_order();
            (agent, order)
        })
        .collect())
}

/// Step cancellations that only follow from another step's failure.
const FAILURE_CANCELLATION_REASONS: &[&str] = &[
    "another step failed",
    "a normal step failed",
    "one or more mission steps failed",
    "the mission run entered cleanup",
    "the owning mission run is terminal",
];

/// Keep the tree's queue labels and order without work presentation enrichment.
/// Read the current generation's pinned definition once for all of this run's steps.
fn enrich_run_step_queues_tx(
    connection: &Connection,
    view: &mut MissionRunView,
) -> rusqlite::Result<()> {
    if view.steps.is_empty() {
        return Ok(());
    }
    let Some(mission) = pinned_generation_mission_tx(connection, &view.generation)? else {
        return Ok(());
    };
    for view in &mut view.steps {
        if let Some(step) = crate::mission::find_step(&mission, &view.step) {
            view.queue.clone_from(&step.queue);
            view.queue_position = step.queue_position;
        }
    }
    Ok(())
}

/// A run's header and each step's effective state, and with `summaries` each step's latest
/// progress and completion summaries. The legacy tree adds queue labels and positions with
/// `enrich_run_step_queues_tx`. Full work presentation, including execution timing, wake
/// messages and harness history, remains in `mission_run_view_tx`.
fn mission_run_steps_view_tx(
    connection: &Connection,
    run_id: &str,
    summaries: bool,
) -> rusqlite::Result<MissionRunView> {
    let mut view = mission_run_header_tx(connection, run_id)?;
    let mut statement = connection.prepare_cached(
        "SELECT subject, run_id, step_path, definition_hash, status, attempt, assignee, available_to, agentless, title, goals, worker_reported,
                lease_owner, lease_incarnation, lease_expires_at_unix_ms, blocked_reason, not_before_unix_ms, created_at_unix_ms, updated_at_unix_ms, readiness_epoch, constraints
         FROM step_runs WHERE generation_id=?1 ORDER BY created_at_unix_ms, step_path",
    )?;
    view.steps = statement
        .query_map(
            [generation_id_from_subject(&view.generation)],
            step_run_from_row,
        )?
        .collect::<Result<Vec<_>, _>>()?;
    apply_step_states_tx(connection, &mut view.steps, summaries)?;
    view.scheduler_fault = first_readiness_fault_tx(connection, &view.subject)?;
    Ok(view)
}

/// Whether an earlier observation sent the item under this delivery key to its owner.
fn routed_to_owner_tx(
    transaction: &rusqlite::Transaction<'_>,
    delivery_key: &str,
) -> Result<bool, St3Error> {
    Ok(transaction
        .query_row(
            "SELECT 1 FROM claims WHERE subject=?1 AND kind='message.sent' LIMIT 1",
            [format!("message/route-{}", &delivery_key[..20])],
            |_| Ok(()),
        )
        .optional()
        .map_err(internal)?
        .is_some())
}

/// One published revision of a mission, if this store holds it.
fn mission_revision_spec_tx(
    connection: &Connection,
    mission: &str,
    revision: &str,
) -> Result<Option<MissionSpec>, St3Error> {
    connection
        .query_row(
            "SELECT body FROM mission_revisions WHERE mission_id=?1 AND revision=?2",
            params![mission, revision],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(internal)?
        .map(|body| serde_json::from_str::<MissionSpec>(&body))
        .transpose()
        .map_err(internal)
}

/// Each step's effective state, and with `summaries` its latest progress and completion
/// summaries, as a run's presentation view shows them.
fn apply_step_states_tx(
    connection: &Connection,
    steps: &mut [StepRunView],
    summaries: bool,
) -> rusqlite::Result<()> {
    for step in steps {
        let snapshot_unix_ms = now_ms();
        apply_effective_step_state(connection, step, snapshot_unix_ms)?;
        if summaries {
            enrich_step_summaries_at(connection, step, snapshot_unix_ms)?;
        }
    }
    Ok(())
}
