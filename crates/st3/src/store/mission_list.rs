//! The current missions list as a published view: every mission's place in the list, and the
//! card of each mission it shows. The refresher in `api::published_lists` folds it; this module
//! holds the rows and the reads that say which missions changed between two cuts.

use super::*;

/// Where one mission sits in the current list, as `mission_collection_page` orders and filters
/// it, and until when that holds with no new claim.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MissionKey {
    /// The list's sort time, newest first; a mission with none sorts last.
    pub(crate) order: Option<i64>,
    /// The time a page continuation seeks after.
    pub(crate) page_time: u128,
    pub(crate) visible: bool,
    /// The last moment a recently ended run keeps its mission shown, if that is all that does.
    pub(crate) visible_until: Option<u128>,
}

/// One publication of the current missions list.
#[derive(Clone, Default)]
pub(crate) struct MissionRows {
    /// Every mission the store knows, shown or not.
    pub(crate) keys: HashMap<String, MissionKey>,
    /// The shown missions, in list order.
    pub(crate) order: Vec<String>,
    /// The card of every shown mission.
    pub(crate) cards: HashMap<String, Arc<Value>>,
    /// The runs that wait on a person, which every card's `must_act` reads.
    pub(crate) attention_runs: BTreeSet<String>,
    /// When the attention runs were last read: grace periods can move a run in or out of
    /// them with no new claim.
    pub(crate) attention_read_at_unix_ms: u128,
    /// When the cards were folded: a worker lease that ends after it changes a card.
    pub(crate) folded_at_unix_ms: u128,
    /// The first worker lease to end after the fold, if one does.
    pub(crate) next_lease_end_unix_ms: Option<u128>,
}

impl MissionRows {
    /// Put the shown missions in list order: newest sort time first, missions without one
    /// last, then by ID, as `mission_collection_page` orders them.
    pub(crate) fn sort(&mut self) {
        let mut order = self
            .keys
            .iter()
            .filter(|(_, key)| key.visible)
            .map(|(id, key)| (key.order, id.clone()))
            .collect::<Vec<_>>();
        order.sort_by(|(left_time, left), (right_time, right)| {
            match (left_time, right_time) {
                (Some(left), Some(right)) => right.cmp(left),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            }
            .then_with(|| left.cmp(right))
        });
        self.order = order.into_iter().map(|(_, id)| id).collect();
    }

    /// The first time a shown mission leaves the list with no new claim: just after the last
    /// moment its recent end keeps it shown.
    pub(crate) fn visible_until(&self) -> Option<u128> {
        self.keys.values().filter_map(|key| key.visible_until).min().map(|until| until + 1)
    }
}

/// What claims between two cuts changed in the missions list.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct MissionChanges {
    /// Missions whose key or card any of those claims can change.
    pub(crate) missions: BTreeSet<String>,
    /// Whether one of them can change which runs wait on a person.
    pub(crate) attention: bool,
}

/// Claim kinds that move nothing the attention runs read: work activity and observations.
/// Any other kind, even an unknown one, rereads them, which costs one bounded read.
const ATTENTION_NEUTRAL_KINDS: &[&str] = &[
    "work.renewed",
    "work.progress",
    "work.extended",
    "work.claimed",
    "harness.observed",
    "harness.activity",
    "harness.usage",
    "harness.diagnostic",
    "runtime.observed",
    "transport.observed",
    "workspace.observed",
    "daemon.diagnostic",
    "message.sent",
    "message.staged",
    "message.delivered",
    "message.read",
    "message.closed",
];

/// Whether the SQL pattern `'__st3/%'` matches `id`: two characters of any kind, then `st3/`
/// in any ASCII case. The list hides these system missions.
fn system_mission(id: &str) -> bool {
    let mut characters = id.chars();
    characters.next().is_some()
        && characters.next().is_some()
        && characters.as_str().len() >= 4
        && characters.as_str().is_char_boundary(4)
        && characters.as_str()[..4].eq_ignore_ascii_case("st3/")
}

/// Each mission's key columns. With `?1` a JSON array of mission IDs, only those missions.
fn mission_keys_sql(selected: bool) -> String {
    let filter = if selected {
        "WHERE mission_id IN (SELECT value FROM json_each(?1))"
    } else {
        ""
    };
    format!(
        "WITH ids AS (
            SELECT mission_id FROM mission_definitions {filter}
            UNION SELECT mission_id FROM mission_runs {filter}
         ), run_states AS (
            SELECT mission_id, SUM(status='running') AS running, SUM(status='standing') AS standing
            FROM mission_runs {filter} GROUP BY mission_id
         ), latest AS (
            SELECT mission_id, status, updated_at_unix_ms,
                   ROW_NUMBER() OVER (PARTITION BY mission_id ORDER BY created_at_unix_ms DESC, id DESC) AS rank
            FROM mission_runs {filter}
         )
         SELECT ids.mission_id,
                COALESCE(latest.updated_at_unix_ms,published.accepted_at_unix_ms,'0'),
                CAST(COALESCE(latest.updated_at_unix_ms,published.accepted_at_unix_ms) AS INTEGER),
                CASE WHEN COALESCE(run_states.running,0)>0 THEN 'running'
                     WHEN COALESCE(run_states.standing,0)>0 THEN 'standing'
                     WHEN def.state='retired' THEN 'retired'
                     WHEN latest.status IS NOT NULL THEN latest.status
                     ELSE def.state END,
                latest.status, def.state,
                COALESCE(run_states.running,0)=0 AND COALESCE(run_states.standing,0)=0,
                CAST(latest.updated_at_unix_ms AS INTEGER)
         FROM ids
         LEFT JOIN mission_definitions def ON def.mission_id=ids.mission_id
         LEFT JOIN claims published ON published.id=def.claim_id
         LEFT JOIN run_states ON run_states.mission_id=ids.mission_id
         LEFT JOIN latest ON latest.mission_id=ids.mission_id AND latest.rank=1"
    )
}

impl Store {
    /// The keys of the selected missions, or of every mission, at the reader's cut, as the
    /// current list sees them at `now`. A selected mission the store no longer has gets none.
    pub(crate) fn mission_list_keys(
        &self,
        selected: Option<&BTreeSet<String>>,
        now: u128,
    ) -> Result<HashMap<String, MissionKey>> {
        let ended_since = now.saturating_sub(RECENTLY_ENDED_MS);
        let connection = self.readers.get();
        let mut statement = connection.prepare_cached(&mission_keys_sql(selected.is_some()))?;
        let read = |row: &rusqlite::Row<'_>| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<i64>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, bool>(6)?,
                row.get::<_, Option<i64>>(7)?,
            ))
        };
        let rows = match selected {
            Some(selected) => statement
                .query_map([serde_json::to_string(selected)?], read)?
                .collect::<rusqlite::Result<Vec<_>>>()?,
            None => statement.query_map([], read)?.collect::<rusqlite::Result<Vec<_>>>()?,
        };
        let mut keys = HashMap::with_capacity(rows.len());
        for (id, page_time, order, state, latest, definition, idle, latest_updated) in rows {
            let open = state
                .as_deref()
                .is_some_and(|state| !matches!(state, "completed" | "failed" | "cancelled" | "retired"));
            // A run that failed or was cancelled keeps its mission in view for a while, unless
            // the mission was retired.
            let ended = matches!(latest.as_deref(), Some("failed" | "cancelled"))
                && definition.as_deref() != Some("retired")
                && idle;
            let ended_until = latest_updated
                .filter(|_| ended)
                .map(|updated| (updated.max(0) as u128).saturating_add(RECENTLY_ENDED_MS));
            let recently_ended = ended && latest_updated.is_some_and(|updated| updated as i128 >= ended_since as i128);
            let shown = !system_mission(&id) && (open || recently_ended);
            keys.insert(
                id,
                MissionKey {
                    order,
                    page_time: page_time.parse().unwrap_or(0),
                    visible: shown,
                    visible_until: ended_until.filter(|_| shown && !open),
                },
            );
        }
        Ok(keys)
    }

    /// Which missions the claims after `after` through `through` can change, read in the
    /// caller's snapshot. A card reads its mission's definition and runs, those runs' current
    /// steps and faults, the root runs those steps answer to, and the person asks that block
    /// them; and which runs wait on a person. A claim about any other subject changes no card.
    pub(crate) fn mission_list_changes(&self, after: u64, through: u64) -> Result<MissionChanges> {
        let connection = self.readers.get();
        let mut changes = MissionChanges::default();
        let mut runs = BTreeSet::new();
        let mut steps = BTreeSet::new();
        let mut statement = connection.prepare_cached(
            "SELECT subject, kind, CASE WHEN kind LIKE 'work.person-%'
                    THEN json_extract(body,'$.fields.origin_step') END
             FROM claims WHERE store_index>?1 AND store_index<=?2",
        )?;
        let claims = statement
            .query_map(params![after, through], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (subject, kind, origin) in claims {
            changes.attention |= !ATTENTION_NEUTRAL_KINDS.contains(&kind.as_str());
            if let Some(mission) = subject.strip_prefix("mission/") {
                changes.missions.insert(mission.to_owned());
            } else if let Some(run) = subject.strip_prefix("mission-run/") {
                runs.insert(run.to_owned());
                // A root run's state ends the steps of every run under it.
                if matches!(kind.as_str(), "mission-run.state" | "mission-run.created") {
                    let mut children = connection.prepare_cached(
                        "SELECT id FROM mission_runs WHERE root_run_id=?1 AND id<>?1",
                    )?;
                    for child in children.query_map([run], |row| row.get::<_, String>(0))? {
                        runs.insert(child?);
                    }
                }
            } else if let Some(generation) = subject.strip_prefix("run-generation/") {
                let mut owner = connection
                    .prepare_cached("SELECT run_id FROM run_generations WHERE id=?1")?;
                if let Some(run) = owner.query_row([generation], |row| row.get::<_, String>(0)).optional()? {
                    runs.insert(run);
                }
            } else if subject.starts_with("step-run/") {
                steps.insert(subject);
                // A person's answer to an ask unblocks the step that asked.
                steps.extend(origin);
            } else if subject.starts_with("agent/") && kind == "intent.desired" {
                // A requester's declaration decides whether its asks still block their steps.
                let mut asks = connection.prepare_cached(
                    "SELECT subject, json_extract(body,'$.fields.origin_step') FROM claims
                     WHERE kind='work.person-asked' AND actor=?1",
                )?;
                for ask in asks.query_map([&subject], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
                })? {
                    let (ask, origin) = ask?;
                    steps.insert(ask);
                    steps.extend(origin);
                }
            }
        }
        if !steps.is_empty() {
            let mut owner = connection.prepare_cached("SELECT run_id FROM step_runs WHERE subject=?1")?;
            for step in &steps {
                if let Some(run) = owner.query_row([step], |row| row.get::<_, String>(0)).optional()? {
                    runs.insert(run);
                }
            }
        }
        if !runs.is_empty() {
            let mut owner = connection.prepare_cached("SELECT mission_id FROM mission_runs WHERE id=?1")?;
            for run in &runs {
                if let Some(mission) = owner.query_row([run], |row| row.get::<_, String>(0)).optional()? {
                    changes.missions.insert(mission);
                }
            }
        }
        Ok(changes)
    }

    /// The missions with a current step whose worker lease ended after `after` and by
    /// `through`: their cards show the step ready again with no new claim.
    pub(crate) fn missions_with_leases_ended(&self, after: u128, through: u128) -> Result<BTreeSet<String>> {
        let connection = self.readers.get();
        let mut statement = connection.prepare_cached(
            "SELECT DISTINCT r.mission_id FROM step_runs s INDEXED BY step_runs_lease_index
             JOIN mission_runs r ON r.id=s.run_id
             WHERE s.lease_owner IS NOT NULL
               AND CAST(s.lease_expires_at_unix_ms AS INTEGER)>?1
               AND CAST(s.lease_expires_at_unix_ms AS INTEGER)<=?2",
        )?;
        let missions = statement
            .query_map(
                params![after.min(i64::MAX as u128) as i64, through.min(i64::MAX as u128) as i64],
                |row| row.get::<_, String>(0),
            )?
            .collect::<rusqlite::Result<BTreeSet<_>>>()?;
        Ok(missions)
    }

    /// When the first worker lease that has not ended by `now` ends.
    pub(crate) fn next_lease_end(&self, now: u128) -> Result<Option<u128>> {
        let connection = self.readers.get();
        let end = connection
            .prepare_cached(
                "SELECT MIN(CAST(lease_expires_at_unix_ms AS INTEGER)) FROM step_runs INDEXED BY step_runs_lease_index
                 WHERE lease_owner IS NOT NULL AND CAST(lease_expires_at_unix_ms AS INTEGER)>?1",
            )?
            .query_row([now.min(i64::MAX as u128) as i64], |row| row.get::<_, Option<i64>>(0))?;
        Ok(end.map(|end| end.max(0) as u128))
    }

    /// The missions that own each of `runs`, which may name runs with or without their
    /// `mission-run/` prefix.
    pub(crate) fn missions_of_runs<'a>(
        &self,
        runs: impl IntoIterator<Item = &'a String>,
    ) -> Result<BTreeSet<String>> {
        let connection = self.readers.get();
        let mut owner = connection.prepare_cached("SELECT mission_id FROM mission_runs WHERE id=?1")?;
        let mut missions = BTreeSet::new();
        for run in runs {
            let run = run.strip_prefix("mission-run/").unwrap_or(run);
            if let Some(mission) = owner.query_row([run], |row| row.get::<_, String>(0)).optional()? {
                missions.insert(mission);
            }
        }
        Ok(missions)
    }

    /// The published missions list, while a refresher keeps one.
    pub(crate) fn published_missions(&self) -> Option<Arc<published_list::Publication<MissionRows>>> {
        self.smalltalk.published_missions.newest()
    }

    pub(crate) fn published_missions_list(&self) -> &published_list::PublishedList<MissionRows> {
        &self.smalltalk.published_missions
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_missions_match_the_sql_pattern_exactly() {
        for hidden in ["__st3/loop", "abst3/x", "__ST3/round", "é_st3/x"] {
            assert!(system_mission(hidden), "{hidden}");
        }
        for shown in ["st3/x", "_st3/x", "__st4/x", "fleet/st3/x", "__st3"] {
            assert!(!system_mission(shown), "{shown}");
        }
    }

    #[test]
    fn missions_sort_newest_first_then_by_id_with_untimed_ones_last() {
        let key = |order| MissionKey { order, page_time: 0, visible: true, visible_until: None };
        let mut rows = MissionRows::default();
        for (id, order) in [("b", Some(5)), ("a", Some(5)), ("c", None), ("d", Some(9))] {
            rows.keys.insert(id.into(), key(order));
        }
        rows.keys.insert("hidden".into(), MissionKey { visible: false, ..key(Some(99)) });
        rows.sort();
        assert_eq!(rows.order, ["d", "a", "b", "c"]);
    }
}
