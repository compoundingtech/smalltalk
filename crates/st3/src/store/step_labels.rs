//! Selected step labels on the caller's existing projection snapshot.
//! This accessor adds neither a readiness certificate nor a separate view registry.
use super::*;

// One limit-plus-one agent window, with both held and upcoming previews per card.
pub(crate) const MAX_SELECTED: usize = 501 * AGENT_WORK_PREVIEW_LIMIT * 2;
const QUERY: &str =
    "SELECT s.subject, s.run_id, r.mission_id, s.step_path, s.title, s.goals, s.status,
                    s.updated_at_unix_ms
             FROM step_runs s
             JOIN mission_runs r ON r.id=s.run_id
             WHERE s.subject IN (SELECT value FROM json_each(?1))";

/// Bounded selected labels for composing cards inside an authorized snapshot. The caller
/// must verify complete projection and all card/authority dependencies in that same cut.
#[allow(dead_code)]
pub(crate) fn rows(
    connection: &Connection,
    subjects: &[String],
) -> Result<BTreeMap<String, StepLabel>> {
    anyhow::ensure!(
        subjects.len() <= MAX_SELECTED,
        "step label selected batch bound"
    );
    read(connection, subjects)
}

// The existing public Store reader retains its original accepted batch sizes.
pub(super) fn read(
    connection: &Connection,
    subjects: &[String],
) -> Result<BTreeMap<String, StepLabel>> {
    if subjects.is_empty() {
        return Ok(BTreeMap::new());
    }
    let mut statement = connection.prepare(QUERY)?;
    statement
        .query_map([serde_json::to_string(subjects)?], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
            ))
        })?
        .map(|row| {
            let (subject, run, mission, path, title, goals, status, updated_at) = row?;
            let goals: Vec<String> = serde_json::from_str(&goals)?;
            Ok((
                subject,
                StepLabel {
                    run: format!("mission-run/{run}"),
                    mission: format!("mission/{mission}"),
                    path,
                    title,
                    goal: goals.into_iter().next(),
                    status,
                    updated_at_unix_ms: updated_at.parse()?,
                },
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests;
