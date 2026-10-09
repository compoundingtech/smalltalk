//! Revision transfers selected seats; it does not execute completed work again.
use super::*;

fn plan_completed_agents_tx(
    transaction: &Transaction<'_>,
    current: &MissionRunView,
    next: &MissionSpec,
    compatible: &BTreeSet<String>,
    generation: &str,
) -> Result<Vec<(DesiredSubject, Option<String>)>, St3Error> {
    let selected_run = mission_run_view_tx(transaction, &current.id).map_err(internal)?;
    if selected_run.generation != current.generation
        || selected_run.status != current.status
        || selected_run.phase != current.phase
    {
        return Err(St3Error::new(
            "stale-run-generation",
            "the run changed before revision seat carry",
        ));
    }
    let completed = current
        .steps
        .iter()
        .filter(|view| view.status == "completed" && compatible.contains(&view.step))
        .map(|view| (view.subject.as_str(), view))
        .collect::<BTreeMap<_, _>>();
    let specs = flatten_mission_step_specs(next)
        .into_iter()
        .map(|step| (step.path.as_str(), step))
        .collect::<BTreeMap<_, _>>();
    if is_failed_terminal(&current.status, &current.phase)
        && completed
            .values()
            .any(|view| specs[view.step.as_str()].declarations_kdl.is_some())
    {
        return Err(St3Error::new(
            "unsafe-completed-declaration-carry",
            "a failed run's cleanup may have ended completed declarations; change the declaring step explicitly to execute it again when reopening",
        ));
    }
    let mut query = transaction
        .prepare_cached(
            "SELECT subject, kind, body, member, owner_run, owner_generation, owner_step
         FROM desired WHERE owner_run=?1 AND owner_generation=?2 ORDER BY subject",
        )
        .map_err(internal)?;
    let selected = query
        .query_map(
            params![current.subject, current.generation],
            desired_from_row,
        )
        .map_err(internal)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(internal)?;
    let mut flattened = Vec::new();
    flatten_steps(next, None, &[], &mut flattened);
    let selectors = flattened
        .into_iter()
        .map(|(step, selector, _)| (step.path.as_str(), selector))
        .collect::<BTreeMap<_, _>>();
    let mut planned = Vec::new();
    for mut desired in selected {
        let Some(view) = desired
            .owner_step
            .as_deref()
            .and_then(|owner| completed.get(owner))
        else {
            continue;
        };
        if desired.kind == "stop" {
            continue;
        }
        let spec = specs[view.step.as_str()];
        let refuse = || {
            St3Error::new(
                "unsafe-completed-declaration-carry",
                format!(
                    "completed step `{}` owns `{}`; revision can carry only unchanged agent declarations with stable names and launch configuration. Change the declaring step explicitly to execute it again",
                    view.step, desired.subject
                ),
            )
        };
        if desired.kind != "agent" || desired.member.is_none() || spec.finally {
            return Err(refuse());
        }
        let Some(source) = spec.declarations_kdl.as_deref() else {
            return Err(refuse());
        };
        let mut old_vars = mission_run_variables(current, &current.revision);
        old_vars.extend(BTreeMap::from([
            ("ST_STEP".into(), view.step.clone()),
            ("ST_STEP_RUN".into(), view.subject.clone()),
            ("ST_ATTEMPT".into(), view.attempt.to_string()),
            (
                "ST_ASSIGNEE".into(),
                view.assigned_to.clone().unwrap_or_default(),
            ),
            (
                "ST_PARENT_STEP_RUN".into(),
                crate::mission::parent_step_path(next, &view.step)
                    .map(|p| {
                        format!(
                            "step-run/{}/{p}",
                            generation_id_from_subject(&current.generation)
                        )
                    })
                    .or_else(|| current.parent_step_run.clone())
                    .unwrap_or_default(),
            ),
        ]));
        let mut new_vars = old_vars.clone();
        new_vars.insert("ST_MISSION_REVISION".into(), next.revision.clone());
        new_vars.insert(
            "ST_RUN_GENERATION".into(),
            generation_id_from_subject(generation).into(),
        );
        let successor_step = format!(
            "step-run/{}/{}",
            generation_id_from_subject(generation),
            view.step
        );
        let (assignee, _, _) = interpolate_selector(&selectors[view.step.as_str()], &new_vars)?;
        new_vars.insert("ST_ASSIGNEE".into(), assignee.unwrap_or_default());
        new_vars.insert("ST_STEP_RUN".into(), successor_step.clone());
        if let Some(parent) = crate::mission::parent_step_path(next, &view.step) {
            new_vars.insert(
                "ST_PARENT_STEP_RUN".into(),
                format!(
                    "step-run/{}/{parent}",
                    generation_id_from_subject(generation)
                ),
            );
        }
        // Dynamic names or commands cannot be transferred as the same selected seat. Refuse
        // before commit rather than publishing a generation whose declaration cannot survive.
        if crate::mission::interpolate_kdl(source, &old_vars)?
            != crate::mission::interpolate_kdl(source, &new_vars)?
        {
            return Err(refuse());
        }
        let previous_step = desired.owner_step.clone();
        desired.owner_generation = Some(generation.into());
        desired.owner_step = Some(successor_step);
        if let Some(member) = desired.member.as_mut() {
            for (name, value) in new_vars {
                if member.environment.contains_key(&name) {
                    member.environment.insert(name, value);
                }
            }
        }
        planned.push((desired, previous_step));
    }
    Ok(planned)
}

impl Store {
    /// Check completed declaration carry before the API publishes the candidate revision.
    /// Cutover rechecks under its writer transaction, so this preview grants no authority.
    pub(crate) fn validate_revision_seat_carry(
        &self,
        current: &MissionRunView,
        next: &MissionSpec,
    ) -> Result<(), St3Error> {
        let old = self
            .mission_spec(
                current.mission.trim_start_matches("mission/"),
                Some(&current.revision),
            )
            .map_err(internal)?
            .ok_or_else(|| {
                St3Error::new(
                    "missing-mission-revision",
                    "the current mission revision is unavailable",
                )
            })?;
        let old = crate::mission::run_mission(old, current.after.as_deref())?;
        let next = crate::mission::run_mission(next.clone(), current.after.as_deref())?;
        let compatible = compatible_step_paths(&old, &next);
        let mut compatible = carried_revision_step_paths(&old, &next, &current.steps, compatible);
        if is_failed_terminal(&current.status, &current.phase) {
            retain_reopened_step_paths(&next, &current.steps, &mut compatible);
        }
        let connection = self.readers.get();
        let transaction = connection.unchecked_transaction().map_err(internal)?;
        plan_completed_agents_tx(
            &transaction,
            current,
            &next,
            &compatible,
            "run-generation/00000000000000000000000000000000",
        )?;
        Ok(())
    }
}

pub(super) fn carry_completed_agents_tx(
    transaction: &Transaction<'_>,
    origin: &str,
    current: &MissionRunView,
    next: &MissionSpec,
    compatible: &BTreeSet<String>,
    generation: &str,
    batch_id: Option<&str>,
) -> Result<Vec<String>, St3Error> {
    let planned = plan_completed_agents_tx(transaction, current, next, compatible, generation)?;
    let mut claims = Vec::new();
    for (desired, previous_step) in planned {
        let predecessors = intent_leaves_tx(transaction, &desired.subject).map_err(internal)?;
        let mut body = serde_json::to_value(&desired).map_err(internal)?;
        // Preserve both ownership invalidations in the event, including the old step key.
        body["previous_owner_generation"] = json!(current.generation);
        body["previous_owner_step"] = json!(previous_step);
        let claim = append_claim_tx(
            transaction,
            origin,
            &desired.subject,
            "intent.desired",
            Some("daemon/runtime"),
            &body,
            &predecessors,
            batch_id,
        )
        .map_err(internal)?;
        select_replicated_desired(transaction, &claim, &desired)?;
        claims.push(claim.id);
    }
    Ok(claims)
}
