//! A mission revision's provenance, recorded as a claim once, at its first publication.
use super::*;
use crate::provenance::{Provenance, internal, read, subject};

pub fn record(
    transaction: &Transaction<'_>,
    origin: &str,
    mission: &MissionSpec,
    provenance: &Provenance,
    actor: Option<&str>,
    batch: &str,
) -> Result<Option<ClaimRecord>, St3Error> {
    if read(transaction, &mission.id, &mission.revision)
        .map_err(internal)?
        .is_some()
    {
        return Ok(None);
    }
    let claim = append_claim_tx(transaction, origin, &subject(&mission.id, &mission.revision), "mission.provenance", actor,
        &json!({"fields": {"mission": mission.subject, "revision": mission.revision, "provenance": provenance}}), &[], Some(batch)).map_err(internal)?;
    Ok(Some(claim))
}
