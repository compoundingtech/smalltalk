//! Git is authoritative for change identity and operational state. These records are
//! cooperative scheduling data, not signed gate evidence or permission to move main.

pub mod transport;
pub use crate::config::changes as config;
pub mod service;
pub mod discovery;
pub mod view;

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

pub const PREFIX: &str = "refs/st/changes/v1/";

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Draft,
    Ready,
    Landed,
    Abandoned,
}

/// A runtime binding is provenance, not proof that the lease is still live. The
/// stage must validate it against its authoritative runtime before a transition.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkBinding {
    pub step: String,
    pub claim_id: String,
    pub incarnation: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Claim {
    pub lane: String,
    pub worker: String,
    pub nonce: String,
    pub work: WorkBinding,
    pub expected_source: String,
    pub expected_tree: String,
    pub acquired_at: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrailerStatus {
    Present,
    /// An explicit external import preserves its source commit without a rewrite.
    ExternalMissing,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrMirror {
    pub repository: String,
    pub number: u64,
    pub branch: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    pub schema_version: u32,
    pub change_id: String,
    pub generation: u64,
    pub previous_record: Option<String>,
    pub source_commit: String,
    pub source_tree: String,
    pub base_commit: String,
    pub trailer_status: TrailerStatus,
    pub state: State,
    pub next: Option<String>,
    pub depends_on: Vec<String>,
    pub actor: String,
    pub created_at: String,
    pub updated_at: String,
    pub claim: Option<Claim>,
    pub mirror: Option<PrMirror>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Revision {
    pub oid: String,
    pub record: Record,
}

pub fn change_id(value: &str) -> Result<()> {
    ensure!(
        value.len() == 41
            && value.starts_with('I')
            && value.as_bytes()[1..]
                .iter()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b)),
        "expected Change-Id I followed by 40 lowercase hexadecimal digits"
    );
    Ok(())
}

pub fn new_change_id() -> Result<String> {
    Ok(format!("I{}", random_hex::<20>()?))
}

pub(super) fn random_hex<const N: usize>() -> Result<String> {
    let mut bytes = [0; N];
    getrandom::fill(&mut bytes)
        .map_err(|error| anyhow::anyhow!("change randomness unavailable: {error}"))?;
    Ok(hex::encode(bytes))
}

pub(crate) fn oid(value: &str) -> Result<()> {
    ensure!(
        matches!(value.len(), 40 | 64)
            && value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "expected full lowercase Git object ID"
    );
    Ok(())
}

pub fn lane(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 64
            && value
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"-_".contains(&b)),
        "expected a lane segment of lowercase letters, digits, hyphens or underscores"
    );
    Ok(())
}

impl Record {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == 1,
            "unsupported change record version"
        );
        change_id(&self.change_id)?;
        ensure!(self.generation > 0, "missing record generation");
        oid(&self.source_commit)?;
        oid(&self.source_tree)?;
        oid(&self.base_commit)?;
        if let Some(previous) = &self.previous_record {
            oid(previous)?;
        }
        ensure!(
            (self.generation == 1) == self.previous_record.is_none(),
            "generation/previous-record mismatch"
        );
        ensure!(
            !self.actor.trim().is_empty() && self.actor.len() <= 256,
            "missing or oversized actor label"
        );
        let created = chrono::DateTime::parse_from_rfc3339(&self.created_at)?;
        let updated = chrono::DateTime::parse_from_rfc3339(&self.updated_at)?;
        ensure!(updated >= created, "updated before created");
        let mut dependencies = std::collections::BTreeSet::new();
        ensure!(self.depends_on.len() <= 64, "too many direct dependencies");
        for dependency in &self.depends_on {
            change_id(dependency)?;
            ensure!(
                dependency != &self.change_id && dependencies.insert(dependency),
                "self or duplicate dependency"
            );
        }
        if let Some(next) = &self.next {
            lane(next)?;
            ensure!(
                self.state == State::Ready && self.claim.is_none(),
                "only ready, unclaimed work has a next marker"
            );
        }
        if let Some(claim) = &self.claim {
            lane(&claim.lane)?;
            ensure!(
                self.state == State::Ready && self.next.is_none(),
                "claimed work must be ready without a next marker"
            );
            ensure!(
                !claim.worker.trim().is_empty() && claim.worker.len() <= 256,
                "invalid worker label"
            );
            ensure!(
                claim.nonce.len() == 32
                    && claim
                        .nonce
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                "invalid claim nonce"
            );
            ensure!(
                claim.work.step.starts_with("step-run/")
                    && claim.work.step.len() <= 512
                    && !claim.work.claim_id.is_empty()
                    && claim.work.claim_id.len() <= 128
                    && !claim.work.incarnation.is_empty()
                    && claim.work.incarnation.len() <= 256,
                "missing or oversized runtime claim binding"
            );
            ensure!(
                claim.expected_source == self.source_commit
                    && claim.expected_tree == self.source_tree,
                "claim content differs from record"
            );
            chrono::DateTime::parse_from_rfc3339(&claim.acquired_at)?;
        }
        if let Some(mirror) = &self.mirror {
            let parts: Vec<_> = mirror.repository.split('/').collect();
            ensure!(
                parts.len() == 2
                    && parts.iter().all(|p| !p.is_empty()
                        && p.len() <= 100
                        && *p != "."
                        && *p != ".."
                        && p.bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))),
                "invalid mirror repository"
            );
            ensure!(
                mirror.number > 0 && !mirror.branch.is_empty() && mirror.branch.len() <= 256,
                "invalid mirror mapping"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
