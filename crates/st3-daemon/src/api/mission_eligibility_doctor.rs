//! Diagnose a sustained current eligibility fault without publishing another episode.
use super::*;

pub(crate) const THRESHOLD_MS: u128 = 5 * 60 * 1_000;

/// `items` is the existing live doctor attention/fault snapshot, not historical state at `now`.
/// The clock controls admission age only; fault currentness remains the snapshot's live test.
pub(crate) fn check(
    store: &Store,
    items: &[AttentionItemView],
    now: u128,
) -> anyhow::Result<DoctorCheck> {
    let mut seen = BTreeSet::new();
    let mut failures = Vec::new();
    for item in items {
        if item.kind != "fault"
            || item.mission_run.is_none()
            || item.step.is_none()
            || now.saturating_sub(item.requested_at_unix_ms) < THRESHOLD_MS
            || !seen.insert(item.episode.as_str())
        {
            continue;
        }
        // Exact current episode only: no operational history fold or new fault is needed.
        let Some(claim) = store.claim_by_id(&item.episode)? else {
            continue;
        };
        if claim.kind != "operational.failure"
            || claim.body["fields"]["condition"] != crate::store::MISSING_AGENT_CONDITION
            || item.mission_run.as_deref() != Some(claim.subject.as_str())
        {
            continue;
        }
        failures.push(item);
    }
    failures.sort_by_key(|item| (item.requested_at_unix_ms, &item.episode));
    let mut details = failures
        .iter()
        .take(20)
        .map(|item| {
            let minutes = now.saturating_sub(item.requested_at_unix_ms) / 60_000;
            format!(
                "{} step {} has no eligible agent for {minutes} minutes: {}",
                item.subject,
                item.step.as_deref().unwrap_or_default(),
                item.detail
            )
        })
        .collect::<Vec<_>>();
    if failures.len() > details.len() {
        details.push(format!("and {} more", failures.len() - details.len()));
    }
    Ok(DoctorCheck {
        name: crate::store::MISSING_AGENT_CONDITION.into(),
        status: if failures.is_empty() { "pass" } else { "fail" }.into(),
        message: if failures.is_empty() {
            "no current missing-agent mission fault has lasted five minutes".into()
        } else {
            format!(
                "{} current missing-agent mission faults have lasted at least five minutes; declare a seat or revise its assignment: {}",
                failures.len(),
                details.join("; ")
            )
        },
    })
}
