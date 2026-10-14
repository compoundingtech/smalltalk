//! Invocation-local work metadata. Reads stay fresh; no context survives this call.
use super::*;

const CHUNK: usize = 128;

#[cfg(test)]
thread_local! {
    static MAX_SNAPSHOT_CALL_NS: std::cell::Cell<u128> = const { std::cell::Cell::new(0) };
    static AFTER_OWNERS: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
    static AFTER_SUPPORT: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}

// LEFT JOIN keeps missing owner rows equivalent to the old optional lookup. json_each is
// the outer loop, and every relation is reached through its existing primary key.
const OWNERS: &str = "SELECT requested.key,
    (runs.id IS NOT NULL AND generations.id IS NOT NULL AND roots.id IS NOT NULL),
    runs.status, runs.phase, runs.current_generation_id, generations.status,
    roots.status, roots.phase
    FROM json_each(?1) AS requested
    LEFT JOIN mission_runs AS runs ON runs.id=json_extract(requested.value,'$.run')
    LEFT JOIN run_generations AS generations
      ON generations.id=json_extract(requested.value,'$.generation')
      AND generations.run_id=runs.id
    LEFT JOIN mission_runs AS roots ON roots.id=runs.root_run_id";

const DEFINITIONS: &str = "SELECT requested.value, revisions.body
    FROM json_each(?1) AS requested
    LEFT JOIN run_generations AS generations ON generations.id=requested.value
    LEFT JOIN mission_runs AS runs ON runs.id=generations.run_id
    LEFT JOIN mission_revisions AS revisions
      ON revisions.mission_id=runs.mission_id AND revisions.revision=generations.revision";

const PHASES: &str = "SELECT id, phase FROM mission_runs
    WHERE id IN (SELECT value FROM json_each(?1) AS requested)";

// These are conservative existence proofs, not lifecycle/authority decisions. Any release
// keeps the original handoff reader; any ask on/for the step keeps the response reader.
// The readers still decide attempts and accepted-time/ack boundaries. Responses join an ask;
// orphan answers cannot enrich a view. No proof survives this snapshot or skips a present input.
// The origin index is installed by Store's schema before readers become available.
const SUPPORT: &str = "SELECT requested.key,
    EXISTS(SELECT 1 FROM claims AS released
           WHERE released.subject=requested.value AND released.kind='work.released'),
    (EXISTS(SELECT 1 FROM claims AS asked
            WHERE asked.subject=requested.value AND asked.kind='work.person-asked')
     OR EXISTS(SELECT 1 FROM claims AS origin INDEXED BY claims_person_ask_origin_step_index
               WHERE origin.kind='work.person-asked'
                 AND json_extract(origin.body,'$.fields.origin_step')=requested.value))
    FROM json_each(?1) AS requested";

struct Inputs {
    views: Vec<StepRunView>,
    definitions: BTreeMap<String, Option<String>>,
    phases: BTreeMap<String, String>,
}

struct DefinitionFields {
    queue: Option<String>,
    queue_position: Option<u32>,
    timeout_ms: Option<u64>,
    fresh_context: bool,
}

#[derive(Default)]
struct DefinitionIndex {
    fields: BTreeMap<String, DefinitionFields>,
    missing_reference: Option<String>,
}

impl DefinitionIndex {
    fn from_mission(mission: &MissionSpec) -> Self {
        let mut index = Self::default();
        index.visit(mission);
        index
    }

    // Match find_step's display-order depth-first traversal and its first matching path.
    // Store only the four fields this reader uses; nested mission trees are never cloned.
    fn visit(&mut self, mission: &MissionSpec) -> bool {
        for id in &mission.display_order {
            let Some(step) = mission.steps.get(id) else {
                self.missing_reference = Some(id.clone());
                return false;
            };
            self.fields
                .entry(step.path.clone())
                .or_insert_with(|| DefinitionFields {
                    queue: step.queue.clone(),
                    queue_position: step.queue_position,
                    timeout_ms: step.timeout_ms,
                    fresh_context: step.fresh_context,
                });
            if let Some(nested) = &step.nested_mission
                && !self.visit(nested)
            {
                return false;
            }
        }
        true
    }

    fn get(&self, path: &str) -> Result<Option<&DefinitionFields>> {
        if let Some(fields) = self.fields.get(path) {
            return Ok(Some(fields));
        }
        // An earlier matching path remains valid. A lookup beyond the invalid display
        // reference refuses the whole read, instead of find_step's indexing panic.
        anyhow::ensure!(
            self.missing_reference.is_none(),
            "mission display order references a missing step: {}",
            self.missing_reference.as_deref().unwrap_or_default()
        );
        Ok(None)
    }
}

pub(super) fn read(store: &Store, actor: &str, at: u128) -> Result<Vec<StepRunView>> {
    let actor = normalize_actor(actor, "agent");
    // The previous work_for_reconcile path also passed one clock to every enrichment.
    // All database reads use this cut; potentially large mission JSON is parsed after release.
    #[cfg(test)]
    let snapshot_started = std::time::Instant::now();
    let captured = store.read_snapshot(|_| inputs(store, &actor, at));
    #[cfg(test)]
    MAX_SNAPSHOT_CALL_NS
        .with(|maximum| maximum.set(maximum.get().max(snapshot_started.elapsed().as_nanos())));
    let mut inputs = captured?;
    let mut definitions = BTreeMap::<String, Option<DefinitionIndex>>::new();
    for view in &mut inputs.views {
        // A pending available-to step or a draining run can become visible without joining a
        // new generation. Record every candidate before filtering, as the shared work read does,
        // so its own transition invalidates the seat's work.
        smallclaims::touched::note_read(|| view.subject.clone());
        smallclaims::touched::note_read(|| view.run.clone());
        smallclaims::touched::note_read(|| view.generation.clone());
        let generation = generation_id_from_subject(&view.generation);
        if !definitions.contains_key(generation) {
            let mission = inputs
                .definitions
                .get(generation)
                .and_then(Option::as_ref)
                .map(|body| {
                    serde_json::from_str::<MissionSpec>(body).map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            body.len(),
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    })
                })
                .transpose()?;
            definitions.insert(
                generation.to_owned(),
                mission.as_ref().map(DefinitionIndex::from_mission),
            );
        }
        if let Some(definition) = definitions.get(generation).and_then(Option::as_ref)
            && let Some(step) = definition.get(&view.step)?
        {
            view.queue.clone_from(&step.queue);
            view.queue_position = step.queue_position;
            view.timeout_ms = step.timeout_ms;
            view.fresh_context = step.fresh_context;
        }
    }
    // Match work_at_snapshot_selected's order and filtering, including terminal work used to
    // close old wakes. No definition, terminal step or historical wake is discarded for cost.
    inputs.views.retain(|view| {
        let draining = inputs
            .phases
            .get(view.run.strip_prefix("mission-run/").unwrap_or(&view.run))
            .is_some_and(|phase| phase == "revision-draining");
        (!draining || matches!(view.status.as_str(), "claimed" | "working" | "verifying"))
            && (view.assigned_to.as_deref() == Some(actor.as_str())
                || view.claimant.as_deref() == Some(actor.as_str())
                || (matches!(view.status.as_str(), "ready" | "blocked")
                    && view
                        .available_to
                        .iter()
                        .any(|candidate| candidate == &actor)))
    });
    Ok(inputs.views)
}

fn inputs(store: &Store, actor: &str, at: u128) -> Result<Inputs> {
    let connection = store.readers.get();
    let mut views = connection
        .prepare_cached(&seat_work_at_snapshot_query(false))?
        .query_map(params![actor, true, false], step_run_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let carried = carried_claimants_tx(
        &connection,
        views
            .iter()
            .filter(|view| view.status == "ready" && view.assigned_to.is_some())
            .map(|view| view.subject.as_str()),
    )?;
    let runs = views
        .iter()
        .map(|view| view.run.strip_prefix("mission-run/").unwrap_or(&view.run))
        .collect::<BTreeSet<_>>();
    let phases = connection
        .prepare_cached(PHASES)?
        .query_map([serde_json::to_string(&runs)?], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?
        .collect::<rusqlite::Result<BTreeMap<_, _>>>()?;
    let owners = owners(&connection, &views)?;
    #[cfg(test)]
    if let Some(after) = AFTER_OWNERS.with(|hook| hook.borrow_mut().take()) {
        after();
    }
    let generations = views
        .iter()
        .map(|view| generation_id_from_subject(&view.generation))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let mut definitions = BTreeMap::new();
    for chunk in generations.chunks(CHUNK) {
        definitions.extend(
            connection
                .prepare_cached(DEFINITIONS)?
                .query_map([serde_json::to_string(chunk)?], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?,
        );
    }
    let support = support(&connection, &views)?;
    #[cfg(test)]
    if let Some(after) = AFTER_SUPPORT.with(|hook| hook.borrow_mut().take()) {
        after();
    }
    for ((view, owner), (has_release, has_ask)) in views.iter_mut().zip(owners).zip(support) {
        apply_effective_step_state_with_owner(&connection, view, at, owner)?;
        if has_release {
            adhoc_work::enrich_handoff(&connection, view, at)?;
        }
        if has_ask {
            person_work::enrich_responses(&connection, view)?;
        }
        view.carried_claimant = carried.get(&view.subject).cloned();
    }
    Ok(Inputs {
        views,
        definitions,
        phases,
    })
}

fn support(connection: &Connection, views: &[StepRunView]) -> Result<Vec<(bool, bool)>> {
    let mut support = Vec::with_capacity(views.len());
    for chunk in views.chunks(CHUNK) {
        let subjects = chunk.iter().map(|view| &view.subject).collect::<Vec<_>>();
        let rows = connection
            .prepare_cached(SUPPORT)?
            .query_map([serde_json::to_string(&subjects)?], |row| {
                Ok((
                    row.get::<_, usize>(0)?,
                    row.get::<_, bool>(1)?,
                    row.get::<_, bool>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        anyhow::ensure!(
            rows.len() == chunk.len(),
            "work support batch lost an input row"
        );
        for (expected, (ordinal, release, ask)) in rows.into_iter().enumerate() {
            anyhow::ensure!(
                ordinal == expected,
                "work support batch changed input order"
            );
            support.push((release, ask));
        }
    }
    Ok(support)
}

fn owners(
    connection: &Connection,
    views: &[StepRunView],
) -> Result<Vec<Option<EffectiveStepOwner>>> {
    // Ownership depends on the exact (run, generation) pair, not the step. Read a shared
    // pair once in this invocation's snapshot, then apply its unchanged rules to every step.
    let key = |view: &StepRunView| {
        (
            view.run
                .strip_prefix("mission-run/")
                .unwrap_or(&view.run)
                .to_owned(),
            generation_id_from_subject(&view.generation).to_owned(),
        )
    };
    let contexts = views
        .iter()
        .map(key)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let mut owners = BTreeMap::new();
    for chunk in contexts.chunks(CHUNK) {
        let requested = chunk
            .iter()
            .map(|(run, generation)| {
                json!({
                    "run": run, "generation": generation,
                })
            })
            .collect::<Vec<_>>();
        let rows = connection
            .prepare_cached(OWNERS)?
            .query_map([serde_json::to_string(&requested)?], |row| {
                let owner = if row.get::<_, bool>(1)? {
                    Some((
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                        row.get(7)?,
                    ))
                } else {
                    None
                };
                Ok((row.get::<_, usize>(0)?, owner))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        anyhow::ensure!(
            rows.len() == chunk.len(),
            "work owner batch lost an input row"
        );
        for (expected, (ordinal, owner)) in rows.into_iter().enumerate() {
            anyhow::ensure!(ordinal == expected, "work owner batch changed input order");
            owners.insert(chunk[expected].clone(), owner);
        }
    }
    views
        .iter()
        .map(|view| {
            owners
                .get(&key(view))
                .cloned()
                .context("work owner batch missed a requested context")
        })
        .collect()
}

#[cfg(test)]
mod tests;
