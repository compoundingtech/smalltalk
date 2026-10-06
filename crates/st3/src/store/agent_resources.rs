//! Process-local, immutable agent-card projections. Reduction never holds the cache mutex.
use super::*;
use std::sync::atomic::Ordering;

const REDUCER_VERSION: usize = 1;
const SNAPSHOTS: usize = 8;

#[derive(Default)]
pub(crate) struct AgentResourcesCache {
    entries: VecDeque<Arc<AgentResourcesEntry>>,
    epoch: usize,
}

impl AgentResourcesCache {
    pub(crate) fn clear(&mut self) {
        self.entries.clear();
        self.epoch = self.epoch.wrapping_add(1);
    }
}

struct AgentResourcesEntry {
    store_index: u64,
    agent_status_index: u64,
    local_observation_index: u64,
    reducer_version: usize,
    history: bool,
    rows: BTreeMap<String, Arc<Value>>,
    order: BTreeSet<(String, String)>,
}

impl AgentResourcesEntry {
    fn values(&self) -> Vec<Value> {
        self.order.iter().map(|(_, id)| (*self.rows[id]).clone()).collect()
    }

    fn insert(&mut self, value: Value) {
        let id = value["id"].as_str().unwrap_or_default().to_owned();
        self.remove(&id);
        let name = value["name"].as_str().unwrap_or_default().to_owned();
        self.order.insert((name, id.clone()));
        self.rows.insert(id, Arc::new(value));
    }

    fn remove(&mut self, id: &str) {
        if let Some(old) = self.rows.remove(id) {
            self.order.remove(&(old["name"].as_str().unwrap_or_default().to_owned(), id.to_owned()));
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct AgentResourceDelta<'a> {
    pub(crate) subjects: &'a BTreeSet<String>,
    pub(crate) previous: &'a [&'a Value],
    pub(crate) activity_only: bool,
}

struct Changes {
    subjects: BTreeSet<String>,
    queues: bool,
    activity_only: bool,
}

fn agent_references(value: &Value, subjects: &mut BTreeSet<String>) {
    match value {
        Value::String(subject) if subject.starts_with("agent/") => {
            subjects.insert(subject.clone());
        }
        Value::Array(values) => {
            for value in values { agent_references(value, subjects); }
        }
        Value::Object(fields) => {
            for value in fields.values() { agent_references(value, subjects); }
        }
        _ => {}
    }
}

impl Store {
    /// Number of conservative full card fills in this process, including cold fills.
    pub fn agent_resources_full_fills(&self) -> usize {
        self.smalltalk.agent_resources_full_fills.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(crate) fn invalidate_agent_resources_schema(&self) {
        self.smalltalk.agent_resources_reducer_version.fetch_add(1, Ordering::Relaxed);
    }

    fn agent_resources_reducer_version(&self) -> usize {
        REDUCER_VERSION + self.smalltalk.agent_resources_reducer_version.load(Ordering::Relaxed)
    }

    /// Classify only explicitly understood dependencies. A future claim kind is never
    /// assumed inert, even if its subject happens to have a familiar prefix.
    fn changed_agent_resources(
        &self,
        previous: &AgentResourcesEntry,
        through: u64,
        local_through: u64,
    ) -> Result<Option<Changes>> {
        let connection = self.readers.get();
        let mut statement = connection.prepare_cached(
            "SELECT subject, kind, body, actor FROM claims WHERE store_index>?1 AND store_index<=?2",
        )?;
        let mut changes = Changes {
            subjects: BTreeSet::new(), queues: false, activity_only: true,
        };
        // Local timeline and same-state harness observations need not publish a claim.
        // Their independent frontier must advance even when the store index is unchanged.
        let mut local = connection.prepare_cached(
            "SELECT subject, kind FROM local_observations
             WHERE id>?1 AND id<=?2 AND after_store_index<=?3",
        )?;
        for row in local.query_map(params![previous.local_observation_index, local_through, through], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })? {
            let (subject, kind) = row?;
            if subject.starts_with("agent/") {
                if self.smalltalk.claim_registry().claim(&kind).is_none() { return Ok(None); }
                changes.subjects.insert(subject);
                changes.activity_only = false;
            }
        }
        let mut rows = statement.query(params![previous.store_index, through])?;
        while let Some(row) = rows.next()? {
            let subject: String = row.get(0)?;
            let kind: String = row.get(1)?;
            if self.smalltalk.claim_registry().claim(&kind).is_none() {
                return Ok(None);
            }
            if subject.starts_with("agent/") {
                // Every registered claim about this seat can change its status, revision,
                // desired metadata, placement, suspension, faults or rollout.
                changes.subjects.insert(subject);
                changes.activity_only = false;
                changes.queues |= kind == "agent.queue.moved" || kind == "intent.desired";
                continue;
            }
            if matches!(kind.as_str(), "daemon.started" | "daemon.diagnostic"
                | "message.staged" | "message.delivered" | "message.read" | "message.closed"
                | "subscription.state" | "subscription.mission-requested"
                | "subscription.mission-request-cancelled" | "subscription.mission-request-released"
                | "subscription.mission-started" | "subscription.batched"
                | "subscription.watch-ended" | "subscription.batch-sent") {
                continue;
            }
            if kind == "message.sent" {
                let body: Value = serde_json::from_str(row.get_ref(2)?.as_str()?)?;
                agent_references(&body["fields"]["from"], &mut changes.subjects);
                agent_references(&body["fields"]["to"], &mut changes.subjects);
                continue;
            }
            if subject.starts_with("step-run/") && (kind.starts_with("step-run.")
                || kind.starts_with("work.")) {
                changes.queues = true;
                changes.activity_only = false;
                let body: Value = serde_json::from_str(row.get_ref(2)?.as_str()?)?;
                let actor: Option<String> = row.get(3)?;
                agent_references(&body, &mut changes.subjects);
                if let Some(actor) = actor.filter(|actor| actor.starts_with("agent/")) {
                    changes.subjects.insert(actor);
                }
                // State claims need not repeat their assignment. Include current participants
                // when a previously pending step becomes ready for the first time.
                let participants = connection.query_row(
                    "SELECT assignee, lease_owner, available_to FROM step_runs WHERE subject=?1",
                    [&subject], |row| Ok((row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<String>>(1)?, row.get::<_, String>(2)?)),
                ).optional()?;
                if let Some((assignee, claimant, available)) = participants {
                    changes.subjects.extend(assignee.into_iter().chain(claimant));
                    agent_references(&serde_json::from_str::<Value>(&available)?, &mut changes.subjects);
                }
                // Include previous participants even when the new claim removes an assignee
                // or releases a lease. The projection table alone cannot prove removals.
                let mut claims = connection.prepare_cached(
                    "SELECT body, actor FROM claims WHERE subject=?1 AND store_index<=?2",
                )?;
                for row in claims.query_map(params![subject, through], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
                })? {
                    let (body, actor) = row?;
                    agent_references(&serde_json::from_str::<Value>(&body)?, &mut changes.subjects);
                    if let Some(actor) = actor.filter(|actor| actor.starts_with("agent/")) {
                        changes.subjects.insert(actor);
                    }
                }
                // Initial assignments are embedded in run/generation creation, not in
                // step-specific history. Recover them even beyond the bounded card preview.
                let mut initial = connection.prepare_cached(
                    "SELECT claims.body FROM claims JOIN step_runs step
                     ON claims.subject IN ('mission-run/' || step.run_id,
                                           'run-generation/' || step.generation_id)
                     WHERE step.subject=?1 AND claims.store_index<=?2
                       AND claims.kind IN ('mission-run.created', 'run-generation.created')",
                )?;
                for body in initial.query_map(params![subject, through], |row| row.get::<_, String>(0))? {
                    agent_references(&serde_json::from_str::<Value>(&body?)?, &mut changes.subjects);
                }
                // Owned seats can become historical when their origin step terminates.
                let mut owned = connection.prepare_cached(
                    "SELECT subject FROM desired WHERE kind='agent' AND owner_step=?1",
                )?;
                changes.subjects.extend(owned.query_map([&subject], |row| row.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?);
                for (id, card) in &previous.rows {
                    if ["current_work_ids", "upcoming_work_ids"].iter().any(|field|
                        card[*field].as_array().is_some_and(|ids| ids.iter().any(|id| id == &subject)))
                        || card["next_work_id"].as_str() == Some(subject.as_str()) {
                        changes.subjects.insert(id.clone());
                    }
                }
                continue;
            }
            if (subject.starts_with("mission-run/") && kind.starts_with("mission-run."))
                || (subject.starts_with("run-generation/") && kind.starts_with("run-generation.")) {
                // A run transition affects all of its queue participants and owned seats,
                // including previously cached seats removed by the new generation.
                changes.queues = true;
                changes.activity_only = false;
                let run = if let Some(id) = subject.strip_prefix("run-generation/") {
                    connection.query_row("SELECT run_id FROM run_generations WHERE id=?1", [id],
                        |row| row.get::<_, String>(0)).optional()?
                } else {
                    subject.strip_prefix("mission-run/").map(str::to_owned)
                };
                let Some(run) = run else { return Ok(None); };
                let owner = format!("mission-run/{run}");
                let mut roots = connection.prepare_cached(
                    "SELECT id FROM mission_runs WHERE root_run_id=
                     (SELECT root_run_id FROM mission_runs WHERE id=?1)",
                )?;
                let owners = roots.query_map([&run], |row| row.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?
                    .into_iter().map(|id| format!("mission-run/{id}"))
                    .chain(std::iter::once(owner.clone())).collect::<BTreeSet<_>>();
                let mut owned = connection.prepare_cached(
                    "SELECT desired.subject FROM desired JOIN mission_runs owner
                     ON desired.owner_run=('mission-run/' || owner.id)
                     WHERE desired.kind='agent' AND owner.root_run_id=
                     (SELECT root_run_id FROM mission_runs WHERE id=?1)",
                )?;
                changes.subjects.extend(owned.query_map([&run], |row| row.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?);
                let mut participants = connection.prepare_cached(
                    "SELECT assignee, lease_owner, available_to FROM step_runs
                     WHERE run_id IN (SELECT id FROM mission_runs WHERE root_run_id=
                     (SELECT root_run_id FROM mission_runs WHERE id=?1))",
                )?;
                for row in participants.query_map([&run], |row| {
                    Ok((row.get::<_, Option<String>>(0)?, row.get::<_, Option<String>>(1)?, row.get::<_, String>(2)?))
                })? {
                    let (assignee, claimant, available) = row?;
                    changes.subjects.extend(assignee.into_iter().chain(claimant));
                    agent_references(&serde_json::from_str::<Value>(&available)?, &mut changes.subjects);
                }
                // Current tables may have already cleared old claimants. Queue previews are
                // bounded, so a cached card cannot enumerate every participant either.
                let mut historical = connection.prepare_cached(
                    "SELECT claims.body, claims.actor FROM claims JOIN step_runs step
                     ON step.subject=claims.subject
                     WHERE claims.store_index<=?2 AND step.run_id IN
                     (SELECT id FROM mission_runs WHERE root_run_id=
                     (SELECT root_run_id FROM mission_runs WHERE id=?1))",
                )?;
                for row in historical.query_map(params![run, through], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
                })? {
                    let (body, actor) = row?;
                    agent_references(&serde_json::from_str::<Value>(&body)?, &mut changes.subjects);
                    if let Some(actor) = actor.filter(|actor| actor.starts_with("agent/")) {
                        changes.subjects.insert(actor);
                    }
                }
                for (id, card) in &previous.rows {
                    if card["owner_run_id"].as_str().is_some_and(|owner| owners.contains(owner))
                        || card["operational"]["owner_generation"].as_str() == Some(subject.as_str())
                        || ["current_work", "upcoming_work"].iter().any(|field|
                            card[*field].as_array().is_some_and(|items| items.iter().any(|work|
                                work["mission_run_id"].as_str().is_some_and(|owner| owners.contains(owner)))))
                        || card["next_work"]["mission_run_id"].as_str().is_some_and(|owner| owners.contains(owner)) {
                        changes.subjects.insert(id.clone());
                    }
                }
                continue;
            }
            return Ok(None);
        }
        // A party or assignment is a reference, not a seat declaration. Full roster
        // discovery visits claim subjects, so never create phantom historical cards.
        let mut known = connection.prepare_cached(
            "SELECT 1 FROM claims WHERE subject=?1 AND store_index<=?2 LIMIT 1",
        )?;
        for subject in std::mem::take(&mut changes.subjects) {
            if previous.rows.contains_key(&subject)
                || known.query_row(params![subject, through], |_| Ok(()))
                    .optional()?.is_some()
            {
                changes.subjects.insert(subject);
            }
        }
        Ok(Some(changes))
    }

    /// Retain bounded immutable snapshots. Only affected rows are reduced, outside the
    /// global mutex; unchanged JSON is shared across snapshots with a maintained name/id index.
    pub(crate) fn cached_agent_resources(
        &self,
        index: u64,
        history: bool,
        build: impl FnOnce(Option<AgentResourceDelta<'_>>) -> Result<Vec<Value>>,
    ) -> Result<Vec<Value>> {
        let version = self.agent_resources_reducer_version();
        let local_index = self.readers.get().query_row(
            "SELECT COALESCE(MAX(id), 0) FROM local_observations WHERE after_store_index<=?1",
            [index], |row| row.get::<_, u64>(0),
        )?;
        let (previous, epoch) = {
            let cache = self.smalltalk.agent_resources_cache.lock().expect("agent resources cache poisoned");
            (cache.entries.iter().filter(|entry|
                entry.store_index <= index && entry.local_observation_index <= local_index
                    && entry.history == history && entry.reducer_version == version)
                .max_by_key(|entry| (entry.store_index, entry.local_observation_index)).cloned(), cache.epoch)
        };
        if let Some(entry) = previous.as_ref().filter(|entry|
            entry.store_index == index && entry.local_observation_index == local_index) {
            return Ok(entry.values());
        }
        let status_index = self.agent_status_index(index)?;
        let changes = previous.as_ref().map(|previous| self.changed_agent_resources(previous, index, local_index))
            .transpose()?.flatten().filter(|changes| !changes.subjects.is_empty()
                || previous.as_ref().is_some_and(|previous| previous.agent_status_index == status_index));
        let mut entry = AgentResourcesEntry {
            store_index: index, agent_status_index: status_index, local_observation_index: local_index,
            reducer_version: version, history,
            rows: BTreeMap::new(), order: BTreeSet::new(),
        };
        if let (Some(previous), Some(changes)) = (previous, changes) {
            entry.rows = previous.rows.clone();
            entry.order = previous.order.clone();
            if !changes.subjects.is_empty() {
                let prior = if changes.queues { Vec::new() } else {
                    changes.subjects.iter().filter_map(|id| previous.rows.get(id))
                        .map(|card| card.as_ref()).collect::<Vec<_>>()
                };
                let fresh = build(Some(AgentResourceDelta {
                    subjects: &changes.subjects, previous: &prior, activity_only: changes.activity_only,
                }))?;
                for subject in &changes.subjects { entry.remove(subject); }
                for value in fresh { entry.insert(value); }
            }
        } else {
            self.smalltalk.agent_resources_full_fills.fetch_add(1, Ordering::Relaxed);
            for value in build(None)? { entry.insert(value); }
        }
        let entry = Arc::new(entry);
        {
            let mut cache = self.smalltalk.agent_resources_cache.lock().expect("agent resources cache poisoned");
            // Repair/replay can invalidate views during reduction. Never publish the
            // pre-repair result into the new cache epoch.
            if cache.epoch == epoch && self.agent_resources_reducer_version() == version {
                cache.entries.retain(|old| old.store_index != index || old.history != history);
                cache.entries.push_back(entry.clone());
                while cache.entries.len() > SNAPSHOTS { cache.entries.pop_front(); }
            }
        }
        Ok(entry.values())
    }
}
