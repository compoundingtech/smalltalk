//! Readers-first numeric cutover. Readiness is a fresh authenticated response, never
//! a remembered heartbeat or proof that an old telemetry inventory happened to converge.
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use smallclaims::fleet::view::{Acceptance, FleetView, Sender, accept};

pub const NUMERIC_SNAPSHOT_VERSION: u32 = 1;

/// The exact owner snapshot a reader has atomically installed and acknowledged.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NumericSnapshotFrontier {
    pub owner: String,
    pub member_key: String,
    pub member_start: u64,
    pub source_epoch: String,
    pub revision: u64,
    pub digest: String,
}

#[derive(Clone, Debug)]
pub struct VerifiedNumericOwner {
    challenge: String,
    frontier: NumericSnapshotFrontier,
}

impl VerifiedNumericOwner {
    /// Verify the response carrying this exact challenge and frontier before calling.
    pub fn from_signed_response(
        membership: &FleetView,
        sender: &Sender,
        challenge: String,
        frontier: NumericSnapshotFrontier,
    ) -> Result<Self, &'static str> {
        if accept(membership, sender, false, false) != Ok(Acceptance::Member) {
            return Err("numeric-owner-member-proof-required");
        }
        let current = membership.current(&sender.name);
        let member = current[0];
        if frontier.owner != sender.name
            || frontier.member_key != member.member_key
            || frontier.member_start != member.start
            || frontier.source_epoch.is_empty()
            || frontier.digest.is_empty()
            || challenge.is_empty()
        {
            return Err("numeric-owner-frontier-mismatch");
        }
        Ok(Self {
            challenge,
            frontier,
        })
    }
}

/// Returned only after the numeric reader has installed complete current snapshots.
/// An empty snapshot still needs its owner frontier; absence does not mean zero usage.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NumericReaderReadiness {
    /// An unpredictable challenge unique to this barrier attempt, signed with the reply.
    pub challenge: String,
    pub version: u32,
    pub member_key: String,
    pub member_start: u64,
    pub reader_epoch: String,
    pub sources: Vec<NumericSnapshotFrontier>,
}

/// A reply verified against the current authoritative member incarnation. Keep this
/// short-lived: a new barrier attempt must obtain fresh replies from every required reader.
#[derive(Clone, Debug)]
pub struct VerifiedNumericReader {
    name: String,
    readiness: NumericReaderReadiness,
}

impl VerifiedNumericReader {
    /// `sender` must come from verification of the signature over this exact response body.
    /// A fleet-secret-only legacy response cannot establish numeric snapshot readiness.
    pub fn from_signed_response(
        membership: &FleetView,
        sender: &Sender,
        readiness: NumericReaderReadiness,
    ) -> Result<Self, &'static str> {
        if accept(membership, sender, false, false) != Ok(Acceptance::Member) {
            return Err("numeric-reader-member-proof-required");
        }
        let current = membership.current(&sender.name);
        let member = current[0];
        if readiness.version != NUMERIC_SNAPSHOT_VERSION {
            return Err("numeric-reader-version-incompatible");
        }
        if readiness.member_key != member.member_key
            || readiness.member_start != member.start
            || readiness.reader_epoch.is_empty()
        {
            return Err("numeric-reader-incarnation-mismatch");
        }
        Ok(Self {
            name: sender.name.clone(),
            readiness,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct NumericReadinessGap {
    pub reader: String,
    pub source: Option<String>,
    pub reason: &'static str,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct NumericCutoverReadiness {
    pub ready: bool,
    pub gaps: Vec<NumericReadinessGap>,
}

/// Evaluate one explicit barrier against authoritative membership and an exact set of
/// owner snapshots. No capability cache, configured peer list, or prior successful attempt
/// can supply a missing reader. This decision does not execute or authorize a cutover.
pub fn numeric_cutover_readiness(
    membership: &FleetView,
    challenge: &str,
    owners: &[VerifiedNumericOwner],
    readers: &[VerifiedNumericReader],
) -> NumericCutoverReadiness {
    let mut gaps = Vec::new();
    let mut gap = |reader: &str, source: Option<&str>, reason| {
        gaps.push(NumericReadinessGap {
            reader: reader.into(),
            source: source.map(str::to_owned),
            reason,
        });
    };
    if membership.anchor.is_none() {
        gap("fleet", None, "authoritative-membership-unavailable");
    }
    if challenge.is_empty() {
        gap("fleet", None, "readiness-challenge-required");
    }
    let mut members = BTreeMap::new();
    for member in membership.members.iter().filter(|m| m.state != "ended") {
        if member.state != "current" || members.insert(member.name.as_str(), member).is_some() {
            gap(&member.name, None, "member-incarnation-conflicted");
        }
    }
    if members.is_empty() {
        gap("fleet", None, "required-membership-empty");
    }
    let mut snapshots = BTreeMap::new();
    for proof in owners {
        let owner = &proof.frontier;
        if proof.challenge != challenge {
            gap(&owner.owner, None, "owner-reply-from-another-attempt");
        }
        if snapshots.insert(owner.owner.as_str(), owner).is_some() {
            gap(&owner.owner, None, "duplicate-owner-frontier");
        }
        if !members.get(owner.owner.as_str()).is_some_and(|member| {
            member.member_key == owner.member_key
                && member.start == owner.member_start
                && !owner.source_epoch.is_empty()
                && !owner.digest.is_empty()
        }) {
            gap(&owner.owner, None, "owner-frontier-incarnation-mismatch");
        }
    }
    for (name, member) in &members {
        if !snapshots.contains_key(name) {
            gap(name, None, "owner-snapshot-unavailable");
        }
        let matching = readers
            .iter()
            .filter(|reader| reader.name == *name)
            .collect::<Vec<_>>();
        if matching.len() != 1 {
            gap(name, None, "reader-unavailable-or-ambiguous");
            continue;
        }
        let readiness = &matching[0].readiness;
        if readiness.challenge != challenge {
            gap(name, None, "reader-reply-from-another-attempt");
            continue;
        }
        // Membership may change between signature verification and the barrier decision.
        if readiness.member_key != member.member_key || readiness.member_start != member.start {
            gap(name, None, "reader-incarnation-replaced");
            continue;
        }
        if snapshots
            .get(name)
            .is_some_and(|snapshot| snapshot.source_epoch != readiness.reader_epoch)
        {
            gap(name, None, "reader-database-epoch-replaced");
            continue;
        }
        for (owner, snapshot) in &snapshots {
            let installed = readiness
                .sources
                .iter()
                .filter(|source| source.owner == *owner)
                .collect::<Vec<_>>();
            if installed.len() != 1 || installed[0] != *snapshot {
                gap(name, Some(owner), "numeric-snapshot-incomplete-or-replaced");
            }
        }
    }
    NumericCutoverReadiness {
        ready: gaps.is_empty(),
        gaps,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smallclaims::fleet::view::MemberView;

    fn fleet() -> FleetView {
        FleetView {
            anchor: Some("test-anchor".into()),
            members: ["cedar", "birch"]
                .into_iter()
                .map(|name| MemberView {
                    name: name.into(),
                    member_key: format!("key-{name}"),
                    state: "current".into(),
                    mode: "listening".into(),
                    endpoints: vec![],
                    start: 1,
                    end: None,
                    ended: None,
                    removed_by: None,
                    removal_reason: None,
                })
                .collect(),
            legacy_removed: vec![],
        }
    }

    fn snapshots(fleet: &FleetView) -> Vec<NumericSnapshotFrontier> {
        fleet
            .members
            .iter()
            .map(|member| NumericSnapshotFrontier {
                owner: member.name.clone(),
                member_key: member.member_key.clone(),
                member_start: member.start,
                source_epoch: "epoch-one".into(),
                revision: 0,
                digest: "authenticated-empty-snapshot".into(),
            })
            .collect()
    }

    fn readers(
        fleet: &FleetView,
        sources: &[NumericSnapshotFrontier],
    ) -> Vec<VerifiedNumericReader> {
        fleet
            .members
            .iter()
            .map(|member| {
                VerifiedNumericReader::from_signed_response(
                    fleet,
                    &Sender {
                        name: member.name.clone(),
                        member_key: Some(member.member_key.clone()),
                        member_signature_valid: true,
                    },
                    NumericReaderReadiness {
                        challenge: "attempt-one".into(),
                        version: 1,
                        member_key: member.member_key.clone(),
                        member_start: member.start,
                        reader_epoch: sources
                            .iter()
                            .find(|source| source.owner == member.name)
                            .unwrap()
                            .source_epoch
                            .clone(),
                        sources: sources.to_vec(),
                    },
                )
                .unwrap()
            })
            .collect()
    }

    fn owners(fleet: &FleetView, sources: &[NumericSnapshotFrontier]) -> Vec<VerifiedNumericOwner> {
        sources
            .iter()
            .map(|source| {
                VerifiedNumericOwner::from_signed_response(
                    fleet,
                    &Sender {
                        name: source.owner.clone(),
                        member_key: Some(source.member_key.clone()),
                        member_signature_valid: true,
                    },
                    "attempt-one".into(),
                    source.clone(),
                )
                .unwrap()
            })
            .collect()
    }

    #[test]
    fn an_old_or_unreachable_required_reader_blocks_cutover_even_with_empty_usage() {
        let fleet = fleet();
        let snapshots = snapshots(&fleet);
        let mut readers = readers(&fleet, &snapshots);
        assert!(
            numeric_cutover_readiness(&fleet, "attempt-one", &owners(&fleet, &snapshots), &readers)
                .ready
        );
        readers.pop();
        let blocked =
            numeric_cutover_readiness(&fleet, "attempt-one", &owners(&fleet, &snapshots), &readers);
        assert!(!blocked.ready);
        assert!(blocked.gaps.iter().any(|gap| gap.reader == "birch" && gap.reason == "reader-unavailable-or-ambiguous"));
        assert!(!numeric_cutover_readiness(&FleetView::default(), "attempt-one", &[], &[]).ready);
        // Successful replies from a previous barrier do not prove a reconnect is reachable.
        let readers = self::readers(&fleet, &snapshots);
        assert!(
            !numeric_cutover_readiness(
                &fleet,
                "attempt-two",
                &owners(&fleet, &snapshots),
                &readers
            )
            .ready
        );
    }

    #[test]
    fn legacy_proof_downgrade_and_member_replacement_cannot_reuse_readiness() {
        let mut fleet = fleet();
        let snapshots = snapshots(&fleet);
        let readers = readers(&fleet, &snapshots);
        let mut readiness = readers[0].readiness.clone();
        let sender = Sender {
            name: "cedar".into(),
            member_key: None,
            member_signature_valid: false,
        };
        assert!(
            VerifiedNumericReader::from_signed_response(&fleet, &sender, readiness.clone())
                .is_err()
        );
        readiness.version = 0;
        let sender = Sender {
            name: "cedar".into(),
            member_key: Some("key-cedar".into()),
            member_signature_valid: true,
        };
        assert!(VerifiedNumericReader::from_signed_response(&fleet, &sender, readiness).is_err());
        let old_owners = owners(&fleet, &snapshots);
        fleet.members[0].member_key = "replacement-key".into();
        fleet.members[0].start = 20;
        assert!(!numeric_cutover_readiness(&fleet, "attempt-one", &old_owners, &readers).ready);
    }

    #[test]
    fn a_new_source_epoch_or_unacknowledged_revision_requires_a_new_snapshot() {
        let fleet = fleet();
        let mut snapshots = snapshots(&fleet);
        let readers = readers(&fleet, &snapshots);
        snapshots[0].source_epoch = "after-reset".into();
        assert!(
            !numeric_cutover_readiness(
                &fleet,
                "attempt-one",
                &owners(&fleet, &snapshots),
                &readers
            )
            .ready
        );
        let mut readers = self::readers(&fleet, &snapshots);
        assert!(
            numeric_cutover_readiness(&fleet, "attempt-one", &owners(&fleet, &snapshots), &readers)
                .ready
        );
        snapshots[0].revision = 1;
        snapshots[0].digest = "new-current-values".into();
        assert!(
            !numeric_cutover_readiness(
                &fleet,
                "attempt-one",
                &owners(&fleet, &snapshots),
                &readers
            )
            .ready
        );
        // Only one reader installing the new source cannot make fleet-wide readiness true.
        readers[0].readiness.sources = snapshots.clone();
        assert!(
            !numeric_cutover_readiness(
                &fleet,
                "attempt-one",
                &owners(&fleet, &snapshots),
                &readers
            )
            .ready
        );
        readers[1].readiness.sources = snapshots.clone();
        assert!(
            numeric_cutover_readiness(&fleet, "attempt-one", &owners(&fleet, &snapshots), &readers)
                .ready
        );
    }
    #[test]
    fn conflicted_membership_and_ambiguous_snapshot_receipts_never_count_as_ready() {
        let mut fleet = fleet();
        let snapshots = snapshots(&fleet);
        let mut readers = readers(&fleet, &snapshots);
        let owners = owners(&fleet, &snapshots);
        readers[0].readiness.sources.push(snapshots[0].clone());
        assert!(!numeric_cutover_readiness(&fleet, "attempt-one", &owners, &readers).ready);
        readers[0].readiness.sources.pop();
        fleet.members[0].state = "conflicted".into();
        assert!(!numeric_cutover_readiness(&fleet, "attempt-one", &owners, &readers).ready);
        fleet.members.clear();
        assert!(!numeric_cutover_readiness(&fleet, "attempt-one", &[], &[]).ready);
    }
}
