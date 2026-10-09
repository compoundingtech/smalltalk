//! Heal: two nodes that hold the same envelopes but project different graphs find the claims
//! that differ and admit the envelopes that carry them again, on the side that lacks them, with
//! no person and no reset. They compare the digests of the claims each projects by writer range,
//! then by subject within the ranges that differ, then the claims of the subjects that differ.
//! When both project the same claims, the graphs differ in how they were projected, and a replay
//! from nothing decides them.
//!
//! The node that dials asks the questions; the worker carries each question to the peer and its
//! answer back to `heal_next`, which compares the answer with this node's own claims and says
//! what to ask next. Every answer carries what the next comparison needs, so nothing but the
//! running report lives between two questions.

use super::*;
use crate::replication::{
    ClaimRange, ClaimRangeDigest, ClaimSubjectDigest, HealClaim, ReplicationFirstSync,
    ReplicationHealAnswer, ReplicationHealQuery, ReplicationHealReport, ReplicationHealStep,
};

pub const CLAIM_RANGE_DOMAIN: &[u8] = b"st3-heal-claim-range-v1\0";
pub const CLAIM_SUBJECT_DOMAIN: &[u8] = b"st3-heal-claim-subject-v1\0";

/// Writer ranges one question narrows by subject.
pub const HEAL_RANGE_LIMIT: usize = 64;
/// Subjects one question lists claims for.
pub const HEAL_SUBJECT_LIMIT: usize = 256;
/// Envelopes one swap sends in each direction.
pub const HEAL_ENVELOPE_LIMIT: usize = 512;
/// Rounds of narrowing one heal makes while each round still moves claims.
pub const HEAL_ROUND_LIMIT: u32 = 8;
/// A heal report older than this belongs to a heal whose worker went away.
pub const HEAL_SESSION_STALE_MS: u128 = 10 * 60 * 1000;

/// The shortest wait between two heals with one peer, and the longest backoff after heals that
/// changed nothing.
pub const HEAL_RETRY_MS: u128 = 60_000;
pub const HEAL_BACKOFF_MAX_MS: u128 = 60 * 60 * 1000;

/// A replay from nothing holds the store for as long as it takes, 41 seconds on a 2 GB store, so
/// a node replays for heals at most this often, and half as often after each replay that did not
/// make the graphs agree.
pub const HEAL_REPLAY_BACKOFF_MS: u128 = 10 * 60 * 1000;
pub const HEAL_REPLAY_BACKOFF_MAX_MS: u128 = 24 * 60 * 60 * 1000;

/// The claims a projection reads: every claim with its batch, except repaired originals.
pub const PROJECTED_CLAIMS: &str = "FROM claims INDEXED BY claims_batch_claim_id
     JOIN batches ON batches.id=claims.batch_id
     WHERE NOT EXISTS (
         SELECT 1 FROM replica_records INDEXED BY replica_records_repaired_claim
         WHERE replica_records.claim_id=claims.id AND replica_records.state='repaired'
     )";

#[derive(Default)]
pub struct HealState {
    /// When this node last replayed its graph for a heal, and how long it waits until the next.
    pub replayed_at_unix_ms: Option<u128>,
    pub replay_backoff_ms: u128,
    /// Each heal this node is asking, by peer.
    pub sessions: BTreeMap<String, HealSession>,
}

#[derive(Default)]
pub struct HealSession {
    pub report: ReplicationHealReport,
    pub rounds: u32,
    /// The claims the last swap asked for, to check that they arrived.
    pub wanted: Vec<HealClaim>,
    /// This node asked the peer to replay.
    pub asked_replay: bool,
}

/// How long divergence lasts before a heal starts. `ST3_REPLICATION_HEAL_AFTER_MS` changes it,
/// for example to watch a divergence before it heals.
pub fn heal_after_ms() -> u128 {
    static AFTER: std::sync::OnceLock<u128> = std::sync::OnceLock::new();
    *AFTER.get_or_init(|| {
        std::env::var("ST3_REPLICATION_HEAL_AFTER_MS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(REPLICATION_DIVERGED_AFTER_MS)
    })
}

impl PeerSyncProgress {
    /// Whether a heal with this peer should start now. The graphs must have differed since
    /// longer than a peer takes to project what it stored, unless `now_or_never` says this
    /// comparison ends a first sync, which checks at once. Returns true once per backoff.
    pub fn heal_due(&mut self, now: u128, now_or_never: bool) -> bool {
        let Some(since) = self.graph_differs_since_unix_ms else {
            return false;
        };
        if !now_or_never && now.saturating_sub(since) < heal_after_ms() {
            return false;
        }
        if let Some(started) = self.heal_started_at_unix_ms
            && now.saturating_sub(started) < self.heal_backoff_ms.max(HEAL_RETRY_MS)
        {
            return false;
        }
        self.heal_started_at_unix_ms = Some(now);
        true
    }

    pub fn finish_heal(&mut self, report: &ReplicationHealReport) {
        let moved = report.refetched + report.pushed != 0 || report.replayed;
        self.heal_backoff_ms = if report.healed || moved {
            HEAL_RETRY_MS
        } else {
            (self.heal_backoff_ms.max(HEAL_RETRY_MS) * 2).min(HEAL_BACKOFF_MAX_MS)
        };
        self.heal = Some(report.clone());
    }
}

impl Store {
    /// Answer a peer's heal question from this node's claims. A swap admits the envelopes the
    /// peer pushed before it answers, and a replay replays before it answers.
    pub fn heal_answer(
        &self,
        peer: &str,
        query: &ReplicationHealQuery,
    ) -> Result<ReplicationHealAnswer> {
        Ok(match query {
            ReplicationHealQuery::Ranges => {
                let ranges = claim_ranges(&self.readers.get())?;
                ReplicationHealAnswer::Ranges {
                    graph_digest: self.replication_snapshot()?.legacy_graph_digest.clone(),
                    projection_digests: self.replication_snapshot()?.projection_digests.clone(),
                    ranges,
                }
            }
            ReplicationHealQuery::Subjects { ranges } => {
                let ranges = ranges
                    .iter()
                    .take(HEAL_RANGE_LIMIT)
                    .cloned()
                    .collect::<Vec<_>>();
                let subjects = claim_subjects(&self.readers.get(), &ranges)?;
                ReplicationHealAnswer::Subjects { ranges, subjects }
            }
            ReplicationHealQuery::Claims { ranges, subjects } => {
                let ranges = ranges
                    .iter()
                    .take(HEAL_RANGE_LIMIT)
                    .cloned()
                    .collect::<Vec<_>>();
                let subjects = subjects
                    .iter()
                    .take(HEAL_SUBJECT_LIMIT)
                    .cloned()
                    .collect::<Vec<_>>();
                let claims = claims_in(
                    &self.readers.get(),
                    &ranges,
                    &subjects.iter().cloned().collect(),
                )?;
                ReplicationHealAnswer::Claims {
                    ranges,
                    subjects,
                    claims,
                }
            }
            ReplicationHealQuery::Swap { push, want } => {
                let push = &push[..push.len().min(HEAL_ENVELOPE_LIMIT)];
                self.readmit_envelopes(peer, push)?;
                let (admitted, refused) = self.envelope_claim_states(push)?;
                let envelopes = self.held_envelopes(want.iter().take(HEAL_ENVELOPE_LIMIT))?;
                ReplicationHealAnswer::Swapped {
                    admitted,
                    refused,
                    envelopes,
                    graph_digest: self.replication_snapshot()?.legacy_graph_digest.clone(),
                    projection_digests: self.replication_snapshot()?.projection_digests.clone(),
                }
            }
            ReplicationHealQuery::Replay => {
                let replayed = self.replay_graph_for_heal()?;
                ReplicationHealAnswer::Replayed {
                    replayed,
                    graph_digest: self.replication_snapshot()?.legacy_graph_digest.clone(),
                    projection_digests: self.replication_snapshot()?.projection_digests.clone(),
                }
            }
        })
    }

    /// Compare a peer's heal answer with this node's claims, act on the difference, and say
    /// what to ask the peer next, or report the heal.
    pub fn heal_next(
        &self,
        peer: &str,
        answer: ReplicationHealAnswer,
    ) -> Result<ReplicationHealStep> {
        match answer {
            ReplicationHealAnswer::Failed { message } => {
                self.finish_heal(peer, false, Some(message))
            }
            ReplicationHealAnswer::Ranges {
                graph_digest,
                projection_digests,
                ranges: peer_ranges,
            } => {
                self.heal_session(peer, |_| {});
                if self.heal_graph_equal(&graph_digest, &projection_digests)? {
                    return self.finish_heal(peer, true, None);
                }
                let differing = differing_ranges(&claim_ranges(&self.readers.get())?, &peer_ranges);
                if !differing.is_empty() {
                    self.heal_session(peer, |session| {
                        session.report.ranges += differing.len() as u64;
                    });
                    return Ok(ReplicationHealStep::Ask {
                        query: ReplicationHealQuery::Subjects {
                            ranges: differing.into_iter().take(HEAL_RANGE_LIMIT).collect(),
                        },
                    });
                }
                // Both project the same claims, so the graphs differ in how they were
                // projected. A replay from nothing follows the claims alone.
                let replayed = self.heal_session(peer, |session| session.report.replayed);
                if !replayed && self.replay_graph_for_heal()? {
                    self.heal_session(peer, |session| session.report.replayed = true);
                    if self.heal_graph_equal(&graph_digest, &projection_digests)? {
                        return self.finish_heal(peer, true, None);
                    }
                }
                if self.heal_session(peer, |session| {
                    std::mem::replace(&mut session.asked_replay, true)
                }) {
                    return self.finish_heal(
                        peer,
                        false,
                        Some(format!(
                            "this node and {peer} project the same claims but different graphs"
                        )),
                    );
                }
                Ok(ReplicationHealStep::Ask {
                    query: ReplicationHealQuery::Replay,
                })
            }
            ReplicationHealAnswer::Subjects {
                ranges,
                subjects: peer_subjects,
            } => {
                let local = claim_subjects(&self.readers.get(), &ranges)?;
                let differing = differing_subjects(&local, &peer_subjects);
                if differing.is_empty() {
                    return self.finish_heal(
                        peer,
                        false,
                        Some(format!(
                            "the claims {peer} projects changed while this node compared them"
                        )),
                    );
                }
                self.heal_session(peer, |session| {
                    session.report.subjects += differing.len() as u64;
                });
                Ok(ReplicationHealStep::Ask {
                    query: ReplicationHealQuery::Claims {
                        ranges,
                        subjects: differing.into_iter().take(HEAL_SUBJECT_LIMIT).collect(),
                    },
                })
            }
            ReplicationHealAnswer::Claims {
                ranges,
                subjects,
                claims: peer_claims,
            } => {
                let local = claims_in(
                    &self.readers.get(),
                    &ranges,
                    &subjects.iter().cloned().collect(),
                )?;
                let key = |claim: &HealClaim| {
                    (claim.writer.clone(), claim.sequence, claim.claim_id.clone())
                };
                let local_keys = local.iter().map(key).collect::<BTreeSet<_>>();
                let peer_keys = peer_claims.iter().map(key).collect::<BTreeSet<_>>();
                let envelope = |claim: &HealClaim| {
                    claim.envelope_hash.as_ref().map(|hash| ReplicaEnvelopeId {
                        writer: claim.writer.clone(),
                        sequence: claim.sequence,
                        hash: hash.clone(),
                    })
                };
                // A claim a checkpoint dropped here is not missing: its tombstone stands for it,
                // and fetching it back would undo the trim.
                let mut dropped_here = 0;
                let mut wanted = Vec::new();
                {
                    let connection = self.readers.get();
                    for claim in &peer_claims {
                        if local_keys.contains(&key(claim)) || claim.envelope_hash.is_none() {
                            continue;
                        }
                        if checkpoint::claim_tombstoned(&connection, &claim.claim_id)? {
                            dropped_here += 1;
                        } else {
                            wanted.push(claim.clone());
                        }
                    }
                }
                let want = wanted
                    .iter()
                    .filter_map(envelope)
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .take(HEAL_ENVELOPE_LIMIT)
                    .collect::<Vec<_>>();
                let push = local
                    .iter()
                    .filter(|claim| !peer_keys.contains(&key(claim)))
                    .filter_map(envelope)
                    .collect::<BTreeSet<_>>();
                if want.is_empty() && push.is_empty() {
                    let reason = if dropped_here == 0 {
                        format!("the claims {peer} projects changed while this node compared them")
                    } else {
                        format!(
                            "the only claims {peer} projects that this node lacks are ones a \
                             checkpoint dropped here"
                        )
                    };
                    return self.finish_heal(peer, false, Some(reason));
                }
                let push = self.held_envelopes(push.iter().take(HEAL_ENVELOPE_LIMIT))?;
                self.heal_session(peer, |session| session.wanted = wanted);
                Ok(ReplicationHealStep::Ask {
                    query: ReplicationHealQuery::Swap { push, want },
                })
            }
            ReplicationHealAnswer::Swapped {
                admitted,
                refused,
                envelopes,
                graph_digest,
                projection_digests,
            } => {
                self.readmit_envelopes(peer, &envelopes)?;
                let wanted = self.heal_session(peer, |session| std::mem::take(&mut session.wanted));
                let (refetched, missing) = self.projected_claims_among(&wanted)?;
                let rounds = self.heal_session(peer, |session| {
                    session.report.refetched += refetched;
                    session.report.pushed += admitted;
                    session.rounds += 1;
                    session.rounds
                });
                if self.heal_graph_equal(&graph_digest, &projection_digests)? {
                    return self.finish_heal(peer, true, None);
                }
                let mut reasons = Vec::new();
                if let Some(missing) = missing {
                    reasons.push(format!(
                        "this node cannot admit {missing} that {peer} projects"
                    ));
                }
                if let Some(refused) = refused {
                    reasons.push(format!(
                        "{peer} cannot admit {refused} that this node projects"
                    ));
                }
                if refetched + admitted != 0 && rounds < HEAL_ROUND_LIMIT {
                    return Ok(ReplicationHealStep::Ask {
                        query: ReplicationHealQuery::Ranges,
                    });
                }
                let reason = if reasons.is_empty() {
                    format!("the graphs of this node and {peer} still differ")
                } else {
                    reasons.join("; ")
                };
                self.finish_heal(peer, false, Some(reason))
            }
            ReplicationHealAnswer::Replayed {
                replayed,
                graph_digest,
                projection_digests,
            } => {
                self.heal_session(peer, |session| session.report.peer_replayed = replayed);
                if self.heal_graph_equal(&graph_digest, &projection_digests)? {
                    return self.finish_heal(peer, true, None);
                }
                let reason = if replayed {
                    format!(
                        "this node and {peer} both replayed from nothing and still project \
                         different graphs from the same claims"
                    )
                } else {
                    format!(
                        "this node and {peer} project the same claims but different graphs, and \
                         {peer} waits before it replays from nothing again"
                    )
                };
                self.finish_heal(peer, false, Some(reason))
            }
        }
    }

    /// This node's first sync after it joined, if it joined.
    pub fn first_sync(&self) -> Result<Option<ReplicationFirstSync>> {
        let connection = self.readers.get();
        first_sync_tx(&connection)
    }

    /// Start this node's first sync: it ends at the first exchange at which this node holds the
    /// same envelopes as a peer. Matching registries check the graph; mixed builds check the log.
    pub fn begin_first_sync(&self, sponsor: &str) -> Result<()> {
        let connection = self.connection.write();
        save_first_sync(
            &connection,
            &ReplicationFirstSync {
                state: "syncing".into(),
                started_at_unix_ms: now_ms(),
                peer: Some(sponsor.to_owned()),
                ..Default::default()
            },
        )
    }

    /// End a mixed-build first sync at a verified equal wire log. Projection equality is
    /// deliberately left unasserted until both members have the same registry.
    pub fn observe_first_sync_log(
        &self,
        peer: &str,
        envelopes: u64,
        authority_digest: &str,
    ) -> Result<()> {
        let connection = self.connection.write();
        let Some(mut first) = first_sync_tx(&connection)? else {
            return Ok(());
        };
        if matches!(first.state.as_str(), "syncing" | "failed") {
            first.state = "verified".into();
            first.ended_at_unix_ms = Some(now_ms());
            first.peer = Some(peer.to_owned());
            first.envelopes = Some(envelopes);
            first.graph_digest = None;
            first.peer_graph_digest = None;
            first.authority_digest = Some(authority_digest.to_owned());
            first.healed = false;
            first.message = Some("projection comparison waits for matching builds".into());
            save_first_sync(&connection, &first)?;
        }
        Ok(())
    }

    /// Record one graph comparison with `peer` for the first sync. Returns true when the graphs
    /// differ at the end of a first sync, which heals at once.
    pub fn observe_first_sync(
        &self,
        peer: &str,
        equal: bool,
        envelopes: u64,
        graph_digest: &str,
        peer_graph_digest: &str,
    ) -> Result<bool> {
        let connection = self.connection.write();
        let Some(mut first) = first_sync_tx(&connection)? else {
            return Ok(false);
        };
        // Once builds match, replace a log-only verification with the graph comparison.
        if first.authority_digest.take().is_some() {
            first.state = "syncing".into();
            first.ended_at_unix_ms = None;
            first.message = None;
        }
        match (first.state.as_str(), equal) {
            ("syncing" | "failed", true) => {
                first.healed |= first.state == "failed";
                first.state = "verified".into();
                first.ended_at_unix_ms = Some(now_ms());
                first.peer = Some(peer.to_owned());
                first.envelopes = Some(envelopes);
                first.graph_digest = Some(graph_digest.to_owned());
                first.peer_graph_digest = Some(peer_graph_digest.to_owned());
                first.message = None;
                save_first_sync(&connection, &first)?;
                Ok(false)
            }
            ("syncing", false) => {
                first.peer = Some(peer.to_owned());
                first.envelopes = Some(envelopes);
                first.graph_digest = Some(graph_digest.to_owned());
                first.peer_graph_digest = Some(peer_graph_digest.to_owned());
                // The heal that starts now ends the first sync, whatever it finds.
                first.healed = true;
                save_first_sync(&connection, &first)?;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    pub fn heal_graph_equal(
        &self,
        legacy: &str,
        projections: &BTreeMap<String, String>,
    ) -> Result<bool> {
        let snapshot = self.replication_snapshot()?;
        Ok(if projections.is_empty() {
            snapshot.legacy_graph_digest == legacy
        } else {
            snapshot.projection_digests == *projections
        })
    }

    /// Update the running report of the heal this node asks `peer`, starting one if none runs.
    pub fn heal_session<T>(&self, peer: &str, update: impl FnOnce(&mut HealSession) -> T) -> T {
        let now = now_ms();
        let mut state = self.heal.lock().unwrap_or_else(PoisonError::into_inner);
        let session = state.sessions.entry(peer.to_owned()).or_default();
        if now.saturating_sub(session.report.at_unix_ms) > HEAL_SESSION_STALE_MS {
            *session = HealSession {
                report: ReplicationHealReport {
                    at_unix_ms: now,
                    ..Default::default()
                },
                ..Default::default()
            };
        }
        update(session)
    }

    pub fn finish_heal(
        &self,
        peer: &str,
        healed: bool,
        unresolved: Option<String>,
    ) -> Result<ReplicationHealStep> {
        let mut report = self
            .heal
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .sessions
            .remove(peer)
            .map(|session| session.report)
            .unwrap_or_else(|| ReplicationHealReport {
                at_unix_ms: now_ms(),
                ..Default::default()
            });
        report.healed = healed;
        report.unresolved = if healed { None } else { unresolved };
        if healed {
            let mut state = self.heal.lock().unwrap_or_else(PoisonError::into_inner);
            state.replay_backoff_ms = self.heal_replay_backoff_ms;
        }
        self.replication_sync
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(peer.to_owned())
            .or_default()
            .finish_heal(&report);
        {
            let connection = self.connection.write();
            if let Some(mut first) = first_sync_tx(&connection)?
                && first.state == "syncing"
                && first.healed
            {
                if healed {
                    first.state = "verified".into();
                    first.graph_digest = Some(self.current_graph_digest_tx(&connection)?);
                    first.peer_graph_digest = first.graph_digest.clone();
                } else {
                    first.state = "failed".into();
                    first.message = report.unresolved.clone();
                }
                first.ended_at_unix_ms = Some(now_ms());
                first.peer = Some(peer.to_owned());
                save_first_sync(&connection, &first)?;
            }
        }
        Ok(ReplicationHealStep::Done { report })
    }

    pub fn current_graph_digest_tx(&self, connection: &Connection) -> Result<String> {
        graph_digest(connection)
    }

    /// Replay the graph from nothing for a heal, unless this node's replay backoff has not
    /// passed. Returns whether it replayed.
    pub fn replay_graph_for_heal(&self) -> Result<bool> {
        let now = now_ms();
        {
            let state = self.heal.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(at) = state.replayed_at_unix_ms
                && now.saturating_sub(at) < state.replay_backoff_ms.max(self.heal_replay_backoff_ms)
            {
                return Ok(false);
            }
        }
        self.replay_replication_graph()?;
        let mut state = self.heal.lock().unwrap_or_else(PoisonError::into_inner);
        state.replayed_at_unix_ms = Some(now);
        state.replay_backoff_ms = (state.replay_backoff_ms.max(self.heal_replay_backoff_ms) * 2)
            .clamp(self.heal_replay_backoff_ms, HEAL_REPLAY_BACKOFF_MAX_MS);
        Ok(true)
    }

    /// Store envelopes a peer sent for a heal and admit them again, then project. An envelope
    /// counts only when its payload is the one its hash names; it replaces this node's copy,
    /// which a hash-checked copy cannot make worse.
    pub fn readmit_envelopes(&self, relay: &str, envelopes: &[ReplicaEnvelope]) -> Result<()> {
        if envelopes.is_empty() {
            return Ok(());
        }
        let fleet_id = self.bound_fleet()?;
        let mut inserted = false;
        {
            let mut connection = self.connection.write();
            let transaction = connection.transaction()?;
            let now = now_ms().to_string();
            for envelope in envelopes {
                let Ok(bytes) = envelope.payload.bytes() else {
                    continue;
                };
                let hash = replica_envelope_hash(
                    &envelope.writer,
                    envelope.sequence,
                    envelope.previous_hash.as_deref(),
                    envelope.accepted_at_unix_ms,
                    bytes,
                );
                if hash != envelope.hash {
                    continue;
                }
                // A checkpoint dropped this envelope here. Its tombstone already stands for it,
                // as in an exchange.
                if checkpoint::envelope_tombstoned(
                    &transaction,
                    &envelope.writer,
                    envelope.sequence,
                    &envelope.hash,
                )? {
                    continue;
                }
                if let (Some(fleet_id), Some(member_key), Some(signature)) =
                    (&fleet_id, &envelope.member_key, &envelope.signature)
                {
                    store_envelope_signature_tx(
                        &transaction,
                        fleet_id,
                        &envelope.writer,
                        envelope.sequence,
                        &envelope.hash,
                        member_key,
                        signature,
                        &now,
                    )?;
                }
                let held = transaction
                    .prepare_cached(
                        "SELECT 1 FROM replica_envelopes
                         WHERE writer=?1 AND sequence=?2 AND envelope_hash=?3",
                    )?
                    .query_row(
                        params![envelope.writer, envelope.sequence, envelope.hash],
                        |_| Ok(()),
                    )
                    .optional()?
                    .is_some();
                inserted |= !held;
                transaction
                    .prepare_cached(
                        "INSERT INTO replica_envelopes(
                         writer, sequence, envelope_hash, previous_hash, accepted_at_unix_ms,
                         payload, relay, receipt_state, received_at_unix_ms
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'pending', ?8)
                     ON CONFLICT(writer, sequence, envelope_hash) DO UPDATE SET
                         payload=excluded.payload, receipt_state='pending', validation_error=NULL",
                    )?
                    .execute(params![
                        envelope.writer,
                        envelope.sequence,
                        envelope.hash,
                        envelope.previous_hash,
                        envelope.accepted_at_unix_ms.to_string(),
                        envelope.payload,
                        relay,
                        now,
                    ])?;
            }
            transaction.commit()?;
        }
        if inserted {
            self.replica_generation.fetch_add(1, Ordering::AcqRel);
        }
        self.validate_replication_backlog()?;
        self.apply_replication_repairs()?;
        self.project_replication_backlog()?;
        Ok(())
    }

    /// The envelopes of `identities` this node holds; one it no longer holds is left out.
    pub fn held_envelopes<'a>(
        &self,
        identities: impl IntoIterator<Item = &'a ReplicaEnvelopeId>,
    ) -> Result<Vec<ReplicaEnvelope>> {
        let held = {
            let connection = self.readers.get();
            let mut statement = connection.prepare(
                "SELECT 1 FROM replica_envelopes
                 WHERE writer=?1 AND sequence=?2 AND envelope_hash=?3",
            )?;
            let mut held = Vec::new();
            for identity in identities {
                if statement.exists(params![identity.writer, identity.sequence, identity.hash])? {
                    held.push(identity.clone());
                }
            }
            held
        };
        self.replica_envelopes(held)
    }

    /// How many claims of `envelopes` this node projects, and what it cannot admit, by reason.
    pub fn envelope_claim_states(
        &self,
        envelopes: &[ReplicaEnvelope],
    ) -> Result<(u64, Option<String>)> {
        let connection = self.readers.get();
        let mut statement = connection.prepare(
            "SELECT records.state, COALESCE(records.error_code, records.state), COUNT(*)
             FROM replica_records AS records
             WHERE records.writer=?1 AND records.sequence=?2 AND records.envelope_hash=?3
               AND COALESCE(records.kind_hint, '')<>'blob' AND records.state<>'repaired'
             GROUP BY records.state, COALESCE(records.error_code, records.state)",
        )?;
        let mut admitted = 0;
        let mut refused = BTreeMap::<String, u64>::new();
        for envelope in envelopes {
            let rows = statement
                .query_map(
                    params![envelope.writer, envelope.sequence, envelope.hash],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, u64>(2)?,
                        ))
                    },
                )?
                .collect::<Result<Vec<_>, _>>()?;
            for (state, reason, count) in rows {
                if state == "valid" {
                    admitted += count;
                } else {
                    *refused.entry(format!("{state} ({reason})")).or_default() += count;
                }
            }
        }
        Ok((admitted, describe_claim_counts(&refused)))
    }

    /// How many of `claims` this node now projects, and the rest by reason.
    pub fn projected_claims_among(&self, claims: &[HealClaim]) -> Result<(u64, Option<String>)> {
        let connection = self.readers.get();
        let mut projected = connection.prepare(
            "SELECT 1 FROM claims WHERE id=?1 AND NOT EXISTS (
                 SELECT 1 FROM replica_records
                 WHERE replica_records.claim_id=claims.id AND replica_records.state='repaired')",
        )?;
        let mut reason = connection.prepare(
            "SELECT state || ' (' || COALESCE(error_code, state) || ')' FROM replica_records
             WHERE writer=?1 AND sequence=?2 AND envelope_hash=?3 AND state<>'valid'
               AND COALESCE(kind_hint, '')<>'blob'
             ORDER BY position LIMIT 1",
        )?;
        let mut count = 0;
        let mut missing = BTreeMap::<String, u64>::new();
        for claim in claims {
            if projected.exists([&claim.claim_id])? {
                count += 1;
                continue;
            }
            let why = reason
                .query_row(
                    params![
                        claim.writer,
                        claim.sequence,
                        claim.envelope_hash.as_deref().unwrap_or_default()
                    ],
                    |row| row.get::<_, String>(0),
                )
                .optional()?
                .unwrap_or_else(|| "absent".into());
            *missing.entry(why).or_default() += 1;
        }
        Ok((count, describe_claim_counts(&missing)))
    }
}

pub fn describe_claim_counts(counts: &BTreeMap<String, u64>) -> Option<String> {
    let total = counts.values().sum::<u64>();
    (total != 0).then(|| {
        format!(
            "{total} claim{}: {}",
            if total == 1 { "" } else { "s" },
            counts
                .iter()
                .map(|(reason, count)| format!("{count} {reason}"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    })
}

pub fn first_sync_tx(connection: &Connection) -> Result<Option<ReplicationFirstSync>> {
    let value = connection
        .query_row("SELECT value FROM meta WHERE key='first_sync'", [], |row| {
            row.get::<_, String>(0)
        })
        .optional()?;
    Ok(value.and_then(|value| serde_json::from_str(&value).ok()))
}

pub fn save_first_sync(connection: &Connection, first: &ReplicationFirstSync) -> Result<()> {
    connection.execute(
        "INSERT OR REPLACE INTO meta(key, value) VALUES ('first_sync', ?1)",
        [serde_json::to_string(first)?],
    )?;
    Ok(())
}

pub fn digest_claim(digest: &mut Sha256, sequence: u64, claim_id: &str) {
    digest.update(sequence.to_be_bytes());
    digest.update((claim_id.len() as u64).to_be_bytes());
    digest.update(claim_id.as_bytes());
}

pub fn new_digest(domain: &[u8]) -> Sha256 {
    let mut digest = Sha256::new();
    digest.update(domain);
    digest
}

/// One claim of `range_claims`.
pub fn projected_claim(row: &rusqlite::Row<'_>) -> rusqlite::Result<HealClaim> {
    Ok(HealClaim {
        writer: row.get(0)?,
        sequence: row.get(1)?,
        claim_id: row.get(2)?,
        subject: row.get(3)?,
        envelope_hash: row.get(4)?,
    })
}

/// The digest of the claims this node projects from each writer range, in range order.
pub fn claim_ranges(connection: &Connection) -> Result<Vec<ClaimRangeDigest>> {
    let mut statement = connection.prepare(&format!(
        "SELECT batches.origin, batches.replica_sequence, claims.id {PROJECTED_CLAIMS}
         ORDER BY batches.origin, batches.replica_sequence, claims.id"
    ))?;
    let mut rows = statement.query([])?;
    let mut ranges = Vec::new();
    let mut current: Option<(String, u64, u64, Sha256)> = None;
    let finish = |(writer, start, count, digest): (String, u64, u64, Sha256)| ClaimRangeDigest {
        writer,
        start,
        count,
        digest: hex::encode(digest.finalize()),
    };
    while let Some(row) = rows.next()? {
        let writer = row.get::<_, String>(0)?;
        let sequence = row.get::<_, u64>(1)?;
        let claim_id = row.get::<_, String>(2)?;
        let start = replication_bucket_start(sequence);
        match &mut current {
            Some((current_writer, current_start, count, digest))
                if *current_writer == writer && *current_start == start =>
            {
                *count += 1;
                digest_claim(digest, sequence, &claim_id);
            }
            _ => {
                if let Some(done) = current.take() {
                    ranges.push(finish(done));
                }
                let mut digest = new_digest(CLAIM_RANGE_DOMAIN);
                digest_claim(&mut digest, sequence, &claim_id);
                current = Some((writer, start, 1, digest));
            }
        }
    }
    if let Some(done) = current {
        ranges.push(finish(done));
    }
    Ok(ranges)
}

/// The claims this node projects from one writer range, in sequence and claim order.
pub fn range_claims(connection: &Connection, range: &ClaimRange) -> Result<Vec<HealClaim>> {
    let mut statement = connection.prepare_cached(&format!(
        "SELECT batches.origin, batches.replica_sequence, claims.id, claims.subject,
                (SELECT envelope_hash FROM replica_envelopes
                 WHERE replica_envelopes.batch_id=claims.batch_id
                 ORDER BY envelope_hash LIMIT 1)
         {PROJECTED_CLAIMS} AND batches.origin=?1
           AND batches.replica_sequence>=?2 AND batches.replica_sequence<?3
         ORDER BY batches.replica_sequence, claims.id"
    ))?;
    let claims = statement
        .query_map(
            params![
                range.writer,
                range.start,
                range.start.saturating_add(REPLICATION_BUCKET_WIDTH)
            ],
            projected_claim,
        )?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(claims)
}

/// The digest of the claims this node projects about each subject within `ranges`.
pub fn claim_subjects(
    connection: &Connection,
    ranges: &[ClaimRange],
) -> Result<Vec<ClaimSubjectDigest>> {
    let mut subjects = Vec::new();
    for range in ranges {
        let mut by_subject = BTreeMap::<String, (u64, Sha256)>::new();
        for claim in range_claims(connection, range)? {
            let (count, digest) = by_subject
                .entry(claim.subject)
                .or_insert_with(|| (0, new_digest(CLAIM_SUBJECT_DOMAIN)));
            *count += 1;
            digest_claim(digest, claim.sequence, &claim.claim_id);
        }
        subjects.extend(by_subject.into_iter().map(|(subject, (count, digest))| {
            ClaimSubjectDigest {
                writer: range.writer.clone(),
                start: range.start,
                subject,
                count,
                digest: hex::encode(digest.finalize()),
            }
        }));
    }
    Ok(subjects)
}

/// The claims this node projects about `subjects` within `ranges`.
pub fn claims_in(
    connection: &Connection,
    ranges: &[ClaimRange],
    subjects: &BTreeSet<String>,
) -> Result<Vec<HealClaim>> {
    let mut claims = Vec::new();
    for range in ranges {
        claims.extend(
            range_claims(connection, range)?
                .into_iter()
                .filter(|claim| subjects.contains(&claim.subject)),
        );
    }
    Ok(claims)
}

/// The ranges whose digests differ or that only one side has.
pub fn differing_ranges(local: &[ClaimRangeDigest], peer: &[ClaimRangeDigest]) -> Vec<ClaimRange> {
    let index = |ranges: &[ClaimRangeDigest]| {
        ranges
            .iter()
            .map(|range| ((range.writer.clone(), range.start), range.digest.clone()))
            .collect::<BTreeMap<_, _>>()
    };
    let (local, peer) = (index(local), index(peer));
    local
        .keys()
        .chain(peer.keys())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|key| local.get(*key) != peer.get(*key))
        .map(|(writer, start)| ClaimRange {
            writer: writer.clone(),
            start: *start,
        })
        .collect()
}

/// The subjects whose digests differ, or that only one side has, in any range.
pub fn differing_subjects(
    local: &[ClaimSubjectDigest],
    peer: &[ClaimSubjectDigest],
) -> Vec<String> {
    let index = |subjects: &[ClaimSubjectDigest]| {
        subjects
            .iter()
            .map(|subject| {
                (
                    (
                        subject.writer.clone(),
                        subject.start,
                        subject.subject.clone(),
                    ),
                    subject.digest.clone(),
                )
            })
            .collect::<BTreeMap<_, _>>()
    };
    let (local, peer) = (index(local), index(peer));
    local
        .keys()
        .chain(peer.keys())
        .filter(|key| local.get(*key) != peer.get(*key))
        .map(|(_, _, subject)| subject.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}
