//! Agreement on checkpoints: which writers take part, the seal, the verification, and when a
//! checkpoint is stable. Nothing here deletes anything; a stable checkpoint is what a trim may
//! act on.
//!
//! Sections 2 and 3 of `doc/fleet/smalltalk/checkpoint-design`. Participants and stability are
//! pure functions of the claims a node holds, like the membership fold: every node that holds
//! the same claims reaches the same answer, whatever order they arrived in.

use super::checkpoint::{
    CheckpointProof, DropPlan, SealedIdentities, checkpoint_name, newest_due_cut,
};
use super::*;

pub const CHECKPOINT_SEALED: &str = "checkpoint.sealed";
pub const CHECKPOINT_VERIFIED: &str = "checkpoint.verified";
pub const CHECKPOINT_EXCUSED: &str = "checkpoint.excused";

/// The version of this agreement protocol, carried in every seal and verification.
pub const CHECKPOINT_PROTOCOL: u64 = 1;

pub const DAY_MS: u128 = 86_400_000;

/// A checkpoint that is still not stable this long after it became due asks a person to act.
pub const CHECKPOINT_ATTENTION_AFTER_MS: u128 = 3 * DAY_MS;

/// The build a seal or verification names, so status can say which build holds a checkpoint up.
/// `CARGO_PKG_VERSION` alone is the same on every node today.
pub fn checkpoint_build() -> String {
    option_env!("ST3_BUILD_REVISION")
        .unwrap_or(env!("CARGO_PKG_VERSION"))
        .to_owned()
}

/// One admitted `checkpoint.*` claim. Lists of them are in canonical order.
#[derive(Clone, Debug, PartialEq)]
pub struct CheckpointClaim {
    pub id: String,
    pub kind: String,
    pub subject: String,
    /// The writer (origin) of the claim.
    pub writer: String,
    pub actor: Option<String>,
    pub fields: serde_json::Map<String, Value>,
}

impl CheckpointClaim {
    pub fn text(&self, name: &str) -> Option<&str> {
        self.fields.get(name).and_then(Value::as_str)
    }

    pub fn number(&self, name: &str) -> Option<u128> {
        self.fields
            .get(name)
            .and_then(Value::as_u64)
            .map(u128::from)
    }

    pub fn names(&self, name: &str) -> Option<BTreeSet<String>> {
        self.fields
            .get(name)?
            .as_array()?
            .iter()
            .map(|value| value.as_str().map(str::to_owned))
            .collect()
    }

    /// The cut, when the claim's subject names the same checkpoint as its field.
    pub fn cut(&self) -> Option<u128> {
        let cut = self.number("cut_unix_ms")?;
        (checkpoint_name(cut) == self.subject).then_some(cut)
    }

    pub fn seal_terms(&self) -> Option<SealTerms> {
        if self.kind != CHECKPOINT_SEALED {
            return None;
        }
        Some(SealTerms {
            cut_unix_ms: self.cut()?,
            participants: self.names("participants")?,
            sealed_digest: self.text("sealed_digest")?.to_owned(),
            rules_digest: self.text("rules_digest")?.to_owned(),
        })
    }

    pub fn verified_terms(&self) -> Option<VerifiedTerms> {
        if self.kind != CHECKPOINT_VERIFIED {
            return None;
        }
        Some(VerifiedTerms {
            cut_unix_ms: self.cut()?,
            participants: self.names("participants")?,
            sealed_digest: self.text("sealed_digest")?.to_owned(),
            rules_digest: self.text("rules_digest")?.to_owned(),
            drop_digest: self.text("drop_digest")?.to_owned(),
            retained_digest: self.text("retained_digest")?.to_owned(),
            graph_digest: self.text("graph_digest")?.to_owned(),
            reader_digest: self.text("reader_digest")?.to_owned(),
        })
    }

    pub fn is_checkpoint_work(&self) -> bool {
        self.kind == CHECKPOINT_SEALED || self.kind == CHECKPOINT_VERIFIED
    }
}

/// What every participant's newest seal must carry before any of them verifies.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SealTerms {
    pub cut_unix_ms: u128,
    pub participants: BTreeSet<String>,
    pub sealed_digest: String,
    pub rules_digest: String,
}

/// What a verification certifies. A certificate needs every participant's to be identical.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct VerifiedTerms {
    pub cut_unix_ms: u128,
    pub participants: BTreeSet<String>,
    pub sealed_digest: String,
    pub rules_digest: String,
    pub drop_digest: String,
    pub retained_digest: String,
    pub graph_digest: String,
    pub reader_digest: String,
}

/// A stable checkpoint: one verification from each participant, all naming exactly those
/// participants and carrying identical digests.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Certificate {
    pub checkpoint: String,
    pub terms: VerifiedTerms,
    /// Each participant's `checkpoint.verified` claim.
    pub verifications: BTreeMap<String, String>,
}

/// Writers a person has excused and that have published no seal or verification since, in
/// canonical order. A writer that comes back and seals ends its own excusal. An excusal that
/// is not a person's counts for nothing.
pub fn excused_writers(claims: &[CheckpointClaim]) -> BTreeSet<String> {
    let mut excused = BTreeSet::new();
    for claim in claims {
        if claim.kind == CHECKPOINT_EXCUSED {
            let by_person = claim
                .actor
                .as_deref()
                .is_some_and(|actor| actor.starts_with("person/"));
            if by_person && let Some(writer) = claim.text("writer") {
                excused.insert(writer.to_owned());
            }
        } else if claim.is_checkpoint_work() {
            excused.remove(&claim.writer);
        }
    }
    excused
}

/// The participants of a checkpoint: every writer this node has heard of, less those that left
/// the fleet and those a person excused. `known` holds the writers of envelopes and tombstones,
/// configured peers and members. Writers any checkpoint claim names as participants are added
/// here, so the set only grows as claims arrive.
pub fn participants(
    known: &BTreeSet<String>,
    left: &BTreeSet<String>,
    claims: &[CheckpointClaim],
) -> BTreeSet<String> {
    let excused = excused_writers(claims);
    let named = claims
        .iter()
        .filter(|claim| claim.is_checkpoint_work())
        .filter_map(|claim| claim.names("participants"))
        .flatten();
    known
        .iter()
        .cloned()
        .chain(named)
        .filter(|writer| !left.contains(writer) && !excused.contains(writer))
        .collect()
}

/// Each writer's newest seal of `checkpoint`.
pub fn newest_seals(claims: &[CheckpointClaim], checkpoint: &str) -> BTreeMap<String, SealTerms> {
    let mut seals = BTreeMap::new();
    for claim in claims.iter().filter(|claim| claim.subject == checkpoint) {
        if let Some(terms) = claim.seal_terms() {
            seals.insert(claim.writer.clone(), terms);
        }
    }
    seals
}

/// Each writer's first verification of `checkpoint`, with its claim ID. A node verifies a
/// checkpoint at most once, so any later one is ignored.
pub fn first_verifications(
    claims: &[CheckpointClaim],
    checkpoint: &str,
) -> BTreeMap<String, (String, VerifiedTerms)> {
    let mut verifications = BTreeMap::new();
    for claim in claims.iter().filter(|claim| claim.subject == checkpoint) {
        if let Some(terms) = claim.verified_terms() {
            verifications
                .entry(claim.writer.clone())
                .or_insert((claim.id.clone(), terms));
        }
    }
    verifications
}

/// Every certificate of `checkpoint`. Without excusals there is at most one (section 3 of the
/// design). When people excused each side of a partition there can be two, and a node adopts
/// every one.
pub fn certificates(claims: &[CheckpointClaim], checkpoint: &str) -> Vec<Certificate> {
    let mut groups: BTreeMap<VerifiedTerms, BTreeMap<String, String>> = BTreeMap::new();
    for (writer, (id, terms)) in first_verifications(claims, checkpoint) {
        groups.entry(terms).or_default().insert(writer, id);
    }
    groups
        .into_iter()
        .filter_map(|(terms, mut verifications)| {
            verifications.retain(|writer, _| terms.participants.contains(writer));
            let complete = !terms.participants.is_empty()
                && terms
                    .participants
                    .iter()
                    .all(|writer| verifications.contains_key(writer));
            complete.then(|| Certificate {
                checkpoint: checkpoint.to_owned(),
                terms,
                verifications,
            })
        })
        .collect()
}

/// Every stable checkpoint, by cut, with its certificates.
pub fn stable_checkpoints(claims: &[CheckpointClaim]) -> BTreeMap<u128, Vec<Certificate>> {
    let checkpoints = claims
        .iter()
        .filter(|claim| claim.kind == CHECKPOINT_VERIFIED)
        .map(|claim| claim.subject.as_str())
        .collect::<BTreeSet<_>>();
    checkpoints
        .into_iter()
        .filter_map(|checkpoint| {
            let certificates = certificates(claims, checkpoint);
            let cut = certificates.first()?.terms.cut_unix_ms;
            Some((cut, certificates))
        })
        .collect()
}

/// The certificate every node applies for a cut. Without excusals a cut has one. When people
/// excused each side of a partition and both sides certified the cut, every node applies the
/// one with the most participants, then the smallest drop digest. It is a pure function of the
/// certificates, so every node picks the same one, and a node that applied the other side's
/// adopts this one's manifest in its place.
pub fn chosen_certificate(certificates: &[Certificate]) -> Option<&Certificate> {
    certificates.iter().min_by(|left, right| {
        right
            .terms
            .participants
            .len()
            .cmp(&left.terms.participants.len())
            .then_with(|| left.terms.drop_digest.cmp(&right.terms.drop_digest))
            .then_with(|| left.terms.cmp(&right.terms))
    })
}

/// What one pass of checkpoint work needs from outside the store.
#[derive(Clone, Debug)]
pub struct CheckpointContext {
    pub now_unix_ms: u128,
    /// Names of the peers in `config.toml`. Each is a writer this node knows of.
    pub configured_peers: Vec<String>,
    /// A directory for the proof's copy of the store.
    pub scratch: PathBuf,
    /// The person asked to act when a checkpoint waits too long.
    pub reviewer: String,
}

/// What one pass of checkpoint work did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "kebab-case")]
pub enum CheckpointAction {
    Sealed {
        checkpoint: String,
        sealed_digest: String,
        participants: BTreeSet<String>,
    },
    Verified {
        checkpoint: String,
        drop_digest: String,
        dropped_claims: usize,
    },
    ProofFailed {
        checkpoint: String,
        mismatches: Vec<String>,
    },
    AttentionRequested {
        checkpoint: String,
        waiting_for: BTreeSet<String>,
    },
    AttentionWithdrawn {
        attention: String,
    },
    Trimmed {
        checkpoint: String,
        envelopes: usize,
        claims: usize,
    },
    /// The newest stable checkpoint needs a manifest from a peer before this node goes on.
    ManifestNeeded {
        checkpoint: String,
        cut_unix_ms: u128,
    },
    /// A trim stopped because deleting would change the graph. Nothing is sealed until a person
    /// looks.
    TrimGraphChanged {
        checkpoint: String,
    },
}

/// `st replication checkpoint status`: the newest stable checkpoint and the one being agreed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointStatusView {
    pub node: String,
    pub newest_stable: Option<Certificate>,
    /// The newest checkpoint this node trimmed or adopted.
    pub trimmed: Option<String>,
    /// A trim stopped because the graph would change, and waits for a person.
    pub halted: bool,
    pub pending: Option<PendingCheckpointView>,
    pub participants: BTreeSet<String>,
    pub excused: BTreeSet<String>,
    pub left: BTreeSet<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingCheckpointView {
    pub checkpoint: String,
    pub cut_unix_ms: u128,
    pub due_at_unix_ms: u128,
    /// This node's current terms, which every participant's newest seal must match.
    pub terms: SealTerms,
    /// Participants whose newest seal matches `terms`.
    pub sealed: BTreeSet<String>,
    /// Participants with a seal that differs, and how.
    pub disagreeing: BTreeMap<String, String>,
    /// Participants with no seal of this checkpoint.
    pub unsealed: BTreeSet<String>,
    /// Participants that verified, and whether their digests match each other.
    pub verified: BTreeSet<String>,
    pub verifications_agree: bool,
    /// The build each participant's newest seal names.
    pub builds: BTreeMap<String, String>,
}

/// A person's word that a trim which stopped because the graph would change may be left as it
/// is, so checkpoints go on.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CheckpointResumeRequest {
    pub reason: String,
    pub actor: String,
}

/// A person's request that checkpoints stop waiting for an unreachable writer.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CheckpointExcuseRequest {
    pub writer: String,
    pub reason: String,
    pub actor: String,
}

pub fn seal_difference(ours: &SealTerms, theirs: &SealTerms) -> String {
    let mut differences = Vec::new();
    if ours.sealed_digest != theirs.sealed_digest {
        differences.push("sealed envelopes");
    }
    if ours.participants != theirs.participants {
        differences.push("participants");
    }
    if ours.rules_digest != theirs.rules_digest {
        differences.push("rules");
    }
    differences.join(", ")
}

/// The agent that asks for attention about checkpoints on this node.

impl Store {
    /// Every admitted `checkpoint.*` claim, in canonical order.
    pub fn checkpoint_claims(&self) -> Result<Vec<CheckpointClaim>> {
        let connection = self.readers.get();
        let mut statement = connection.prepare_cached(&format!(
            "SELECT {CLAIM_COLUMNS} FROM claims JOIN batches ON batches.id=claims.batch_id
             WHERE claims.kind IN ('{CHECKPOINT_SEALED}', '{CHECKPOINT_VERIFIED}',
                                   '{CHECKPOINT_EXCUSED}')
               AND NOT EXISTS (SELECT 1 FROM replica_records records
                               WHERE records.claim_id=claims.id AND records.state != 'valid')
             ORDER BY {CANONICAL_ORDER}"
        ))?;
        let claims = statement
            .query_map([], claim_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(claims
            .into_iter()
            .map(|claim| CheckpointClaim {
                fields: claim
                    .body
                    .get("fields")
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default(),
                id: claim.id,
                kind: claim.kind,
                subject: claim.subject,
                writer: claim.origin,
                actor: claim.actor,
            })
            .collect())
    }

    /// Every writer this node has heard of: the writers of the envelopes and tombstones it
    /// holds, itself, its configured peers, and every member of the membership fold.
    pub fn checkpoint_known_writers(
        &self,
        configured_peers: &[String],
    ) -> Result<BTreeSet<String>> {
        let mut known = {
            let connection = self.readers.get();
            connection
                .prepare_cached(
                    "SELECT DISTINCT writer FROM replica_envelopes
                     UNION SELECT DISTINCT writer FROM checkpoint_envelopes",
                )?
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<BTreeSet<_>>>()?
        };
        known.insert(self.origin.clone());
        known.extend(configured_peers.iter().cloned());
        let membership = self.fleet_membership()?;
        known.extend(
            membership
                .incarnations()
                .map(|incarnation| incarnation.name.clone()),
        );
        Ok(known)
    }

    /// Writers that left the fleet: every incarnation ended with a drained `leave`. A removal
    /// does not count; see section 3 of the design.
    pub fn checkpoint_left_writers(&self) -> Result<BTreeSet<String>> {
        let membership = self.fleet_membership()?;
        let names = membership
            .incarnations()
            .map(|incarnation| incarnation.name.clone())
            .collect::<BTreeSet<_>>();
        Ok(names
            .into_iter()
            .filter(|name| {
                matches!(
                    membership.state(name),
                    crate::fleet::MemberState::Ended(incarnation)
                        if incarnation.ended.as_deref() == Some("left")
                )
            })
            .collect())
    }

    pub fn checkpoint_participants(
        &self,
        claims: &[CheckpointClaim],
        configured_peers: &[String],
    ) -> Result<(BTreeSet<String>, BTreeSet<String>)> {
        let known = self.checkpoint_known_writers(configured_peers)?;
        let left = self.checkpoint_left_writers()?;
        Ok((participants(&known, &left, claims), left))
    }

    /// Record that this node sealed or verified a checkpoint. From here on it never writes a
    /// batch or claim dated before the cut; see `write_time`.
    pub fn record_checkpoint_state(
        &self,
        checkpoint: &str,
        cut_unix_ms: u128,
        state: &str,
        sealed: &SealedIdentities,
        plan: Option<&DropPlan>,
    ) -> Result<()> {
        let connection = self.connection.write();
        connection.execute(
            "INSERT INTO checkpoints(id, cut_unix_ms, state, seal_rowid, sealed_digest,
                                     drop_digest, updated_at_unix_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(id) DO UPDATE SET state=excluded.state, seal_rowid=excluded.seal_rowid,
                 sealed_digest=excluded.sealed_digest, drop_digest=excluded.drop_digest,
                 updated_at_unix_ms=excluded.updated_at_unix_ms",
            params![
                checkpoint,
                i64::try_from(cut_unix_ms)?,
                state,
                sealed.seal_rowid,
                sealed.digest,
                plan.map(|plan| plan.drop_digest.clone()),
                i64::try_from(now_ms())?,
            ],
        )?;
        Ok(())
    }

    /// Idempotency keys are fleet-wide, and two nodes can seal or verify with identical fields,
    /// so every key names this node.
    pub fn append_checkpoint_claim(
        &self,
        checkpoint: &str,
        kind: &str,
        fields: BTreeMap<String, Value>,
        key: String,
    ) -> Result<()> {
        self.append_claim(&ClaimInput {
            subject: checkpoint.to_owned(),
            kind: kind.to_owned(),
            actor: None,
            fields,
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some(key),
        })
        .map_err(|error| anyhow::anyhow!("{}: {}", error.code, error.message))?;
        Ok(())
    }

    pub fn publish_seal(
        &self,
        checkpoint: &str,
        terms: &SealTerms,
        sealed: &SealedIdentities,
    ) -> Result<()> {
        // The floor first: the seal itself, and everything after it, is dated at or after the
        // cut, even on a clock that runs days slow.
        self.record_checkpoint_state(checkpoint, terms.cut_unix_ms, "sealed", sealed, None)?;
        self.append_checkpoint_claim(
            checkpoint,
            CHECKPOINT_SEALED,
            BTreeMap::from([
                ("cut_unix_ms".into(), json!(terms.cut_unix_ms)),
                ("participants".into(), json!(terms.participants)),
                ("sealed_digest".into(), json!(terms.sealed_digest)),
                ("sealed_count".into(), json!(sealed.count)),
                ("rules_digest".into(), json!(terms.rules_digest)),
                ("checkpoint_protocol".into(), json!(CHECKPOINT_PROTOCOL)),
                ("build".into(), json!(checkpoint_build())),
            ]),
            format!(
                "checkpoint-sealed:{}:{checkpoint}:{}",
                self.origin,
                terms_key(terms)
            ),
        )
    }

    pub fn publish_verification(
        &self,
        checkpoint: &str,
        terms: &SealTerms,
        plan: &DropPlan,
        proof: &CheckpointProof,
    ) -> Result<()> {
        self.append_checkpoint_claim(
            checkpoint,
            CHECKPOINT_VERIFIED,
            BTreeMap::from([
                ("cut_unix_ms".into(), json!(terms.cut_unix_ms)),
                ("participants".into(), json!(terms.participants)),
                ("sealed_digest".into(), json!(terms.sealed_digest)),
                ("rules_digest".into(), json!(terms.rules_digest)),
                ("drop_digest".into(), json!(plan.drop_digest)),
                ("dropped_envelopes".into(), json!(plan.envelopes.len())),
                ("dropped_claims".into(), json!(plan.claims.len())),
                ("retained_digest".into(), json!(plan.retained_digest)),
                ("graph_digest".into(), json!(proof.graph_digest)),
                ("reader_digest".into(), json!(proof.reader_digest)),
                ("checkpoint_protocol".into(), json!(CHECKPOINT_PROTOCOL)),
                ("build".into(), json!(checkpoint_build())),
            ]),
            format!("checkpoint-verified:{}:{checkpoint}", self.origin),
        )
    }

    /// One pass of checkpoint work: seal the newest due checkpoint, or verify it once every
    /// participant's seal matches. A node that is catching up with a peer does nothing, since
    /// what it holds before the cut is still changing. Call it after exchanges and every few
    /// minutes. The proof copies the store and replays it, so run it off the request path.
    pub fn checkpoint_step(&self, context: &CheckpointContext) -> Result<Vec<CheckpointAction>> {
        let mut actions = Vec::new();
        if self.replication_catching_up() {
            return Ok(actions);
        }
        let claims = self.checkpoint_claims()?;
        // Every node trims the same checkpoints in the same order, so the newest stable one is
        // applied here before this node seals or verifies a newer one.
        if let Some(need) = self.apply_stable_checkpoints(&claims, &mut actions)? {
            actions.push(CheckpointAction::ManifestNeeded {
                checkpoint: need.checkpoint,
                cut_unix_ms: need.cut_unix_ms,
            });
            return Ok(actions);
        }
        if self.checkpoint_halted()? {
            return Ok(actions);
        }
        let stable = stable_checkpoints(&claims);
        let newest_stable = stable.keys().next_back().copied();
        let cut = newest_due_cut(context.now_unix_ms);
        if cut == 0 || newest_stable.is_some_and(|stable| stable >= cut) {
            return Ok(actions);
        }
        let checkpoint = checkpoint_name(cut);
        let (participants, left) =
            self.checkpoint_participants(&claims, &context.configured_peers)?;
        if left.contains(&self.origin) {
            return Ok(actions);
        }
        // Sealing ends this node's own excusal, so it counts itself.
        let mut participants = participants;
        participants.insert(self.origin.clone());
        let verified = first_verifications(&claims, &checkpoint);
        let seals = newest_seals(&claims, &checkpoint);
        if !verified.contains_key(&self.origin) {
            let sealed = self.checkpoint_sealed_identities(cut, None)?;
            let terms = SealTerms {
                cut_unix_ms: cut,
                participants,
                sealed_digest: sealed.digest.clone(),
                rules_digest: self.runtime.checkpoint_rules_digest(),
            };
            if seals.get(&self.origin) != Some(&terms) {
                self.publish_seal(&checkpoint, &terms, &sealed)?;
                actions.push(CheckpointAction::Sealed {
                    checkpoint: checkpoint.clone(),
                    sealed_digest: terms.sealed_digest.clone(),
                    participants: terms.participants.clone(),
                });
            } else if terms
                .participants
                .iter()
                .all(|writer| seals.get(writer) == Some(&terms))
            {
                self.verify_checkpoint(
                    &checkpoint,
                    &terms,
                    sealed.seal_rowid,
                    context,
                    &mut actions,
                )?;
            }
        }
        Ok(actions)
    }

    pub fn verify_checkpoint(
        &self,
        checkpoint: &str,
        terms: &SealTerms,
        seal_rowid: i64,
        context: &CheckpointContext,
        actions: &mut Vec<CheckpointAction>,
    ) -> Result<()> {
        let (sealed, plan, proof) =
            self.plan_checkpoint_through(terms.cut_unix_ms, Some(seal_rowid), &context.scratch)?;
        if plan.sealed_digest != terms.sealed_digest {
            // Something before the cut arrived since the seal; the next pass seals again.
            return Ok(());
        }
        if !proof.passed {
            self.append_claim(&ClaimInput {
                subject: format!("daemon/{}", self.origin),
                kind: "daemon.diagnostic".into(),
                actor: None,
                fields: BTreeMap::from([
                    ("severity".into(), json!("error")),
                    ("code".into(), json!("checkpoint-proof-failed")),
                    (
                        "reason".into(),
                        json!(format!(
                            "dropping {} claims before {checkpoint} would change {}",
                            plan.claims.len(),
                            proof.mismatches.join("; ")
                        )),
                    ),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some(format!(
                    "checkpoint-proof-failed:{}:{checkpoint}:{}",
                    self.origin, plan.drop_digest
                )),
            })
            .map_err(|error| anyhow::anyhow!("{}: {}", error.code, error.message))?;
            actions.push(CheckpointAction::ProofFailed {
                checkpoint: checkpoint.to_owned(),
                mismatches: proof.mismatches,
            });
            return Ok(());
        }
        self.record_checkpoint_state(
            checkpoint,
            terms.cut_unix_ms,
            "verified",
            &SealedIdentities::of(&sealed),
            Some(&plan),
        )?;
        self.publish_verification(checkpoint, terms, &plan, &proof)?;
        actions.push(CheckpointAction::Verified {
            checkpoint: checkpoint.to_owned(),
            drop_digest: plan.drop_digest.clone(),
            dropped_claims: plan.claims.len(),
        });
        Ok(())
    }

    /// Whether a trim stopped because the graph would change. Checkpoints then wait for a
    /// person; see `resume_checkpoints`.
    pub fn checkpoint_halted(&self) -> Result<bool> {
        Ok(self
            .readers
            .get()
            .query_row(
                "SELECT 1 FROM checkpoints WHERE state='graph-changed' LIMIT 1",
                [],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    /// A person looked at a trim that stopped because the graph would change. The rows stay
    /// as they are, and checkpoints go on from the next one.
    pub fn resume_checkpoints(&self, actor: &str, reason: &str) -> Result<(), St3Error> {
        let actor = normalize_actor(actor, "person");
        if !actor.starts_with("person/") || reason.trim().is_empty() {
            return Err(St3Error::new(
                "invalid-checkpoint-resume",
                "a person resumes checkpoints, with a reason",
            ));
        }
        let connection = self.connection.write();
        connection
            .execute(
                "UPDATE checkpoints SET state='set-aside', detail=?1, updated_at_unix_ms=?2
                 WHERE state='graph-changed'",
                params![
                    json!({"resumed_by": actor, "reason": reason}).to_string(),
                    i64::try_from(now_ms()).unwrap_or(i64::MAX)
                ],
            )
            .map_err(internal)?;
        Ok(())
    }

    /// What `st replication checkpoint status` shows.
    pub fn checkpoint_status(
        &self,
        now_unix_ms: u128,
        configured_peers: &[String],
    ) -> Result<CheckpointStatusView> {
        let claims = self.checkpoint_claims()?;
        let stable = stable_checkpoints(&claims);
        let newest_stable = stable
            .iter()
            .next_back()
            .and_then(|(_, certificates)| certificates.first().cloned());
        let (mut participants, left) = self.checkpoint_participants(&claims, configured_peers)?;
        participants.insert(self.origin.clone());
        let cut = newest_due_cut(now_unix_ms);
        let pending = (cut > 0
            && newest_stable
                .as_ref()
                .is_none_or(|stable| stable.terms.cut_unix_ms < cut))
        .then(|| -> Result<PendingCheckpointView> {
            let checkpoint = checkpoint_name(cut);
            let sealed = self.checkpoint_sealed_identities(cut, None)?;
            let terms = SealTerms {
                cut_unix_ms: cut,
                participants: participants.clone(),
                sealed_digest: sealed.digest,
                rules_digest: self.runtime.checkpoint_rules_digest(),
            };
            let seals = newest_seals(&claims, &checkpoint);
            let verified = first_verifications(&claims, &checkpoint);
            let builds = claims
                .iter()
                .filter(|claim| claim.subject == checkpoint && claim.kind == CHECKPOINT_SEALED)
                .filter_map(|claim| Some((claim.writer.clone(), claim.text("build")?.to_owned())))
                .collect();
            Ok(PendingCheckpointView {
                due_at_unix_ms: cut + 2 * DAY_MS,
                sealed: participants
                    .iter()
                    .filter(|writer| seals.get(*writer) == Some(&terms))
                    .cloned()
                    .collect(),
                disagreeing: participants
                    .iter()
                    .filter_map(|writer| {
                        let theirs = seals.get(writer)?;
                        (theirs != &terms)
                            .then(|| (writer.clone(), seal_difference(&terms, theirs)))
                    })
                    .collect(),
                unsealed: participants
                    .iter()
                    .filter(|writer| !seals.contains_key(*writer))
                    .cloned()
                    .collect(),
                verifications_agree: verified
                    .values()
                    .map(|(_, terms)| terms)
                    .collect::<BTreeSet<_>>()
                    .len()
                    <= 1,
                verified: verified.keys().cloned().collect(),
                builds,
                checkpoint,
                cut_unix_ms: cut,
                terms,
            })
        })
        .transpose()?;
        let trimmed = self
            .readers
            .get()
            .query_row(
                "SELECT id FROM checkpoints WHERE state='trimmed' ORDER BY cut_unix_ms DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()?;
        Ok(CheckpointStatusView {
            node: self.origin.clone(),
            newest_stable,
            trimmed,
            halted: self.checkpoint_halted()?,
            pending,
            excused: excused_writers(&claims),
            participants,
            left,
        })
    }

    /// A person excuses an unreachable writer, so checkpoints stop waiting for it. This fences
    /// nothing: whatever the writer wrote while away still replicates when it returns, and its
    /// next seal ends the excusal.
    pub fn excuse_checkpoint_writer(
        &self,
        request: &CheckpointExcuseRequest,
    ) -> Result<ClaimRecord, St3Error> {
        let actor = normalize_actor(&request.actor, "person");
        if !actor.starts_with("person/") {
            return Err(St3Error::new(
                "invalid-checkpoint-excusal",
                "only a person can excuse a writer from checkpoints",
            ));
        }
        if request.writer.trim().is_empty() || request.reason.trim().is_empty() {
            return Err(St3Error::new(
                "invalid-checkpoint-excusal",
                "an excusal needs a writer and a reason",
            ));
        }
        if request.writer == self.origin {
            return Err(St3Error::new(
                "invalid-checkpoint-excusal",
                "a node cannot excuse itself; excuse it from another machine",
            ));
        }
        self.append_claim(&ClaimInput {
            // One subject per excused writer, from its name, which a person typed.
            subject: format!(
                "checkpoint-excusal/{}",
                &hex::encode(Sha256::digest(request.writer.as_bytes()))[..16]
            ),
            kind: CHECKPOINT_EXCUSED.into(),
            actor: Some(actor),
            fields: BTreeMap::from([
                ("writer".into(), json!(request.writer)),
                ("reason".into(), json!(request.reason)),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
    }

    /// Shift the clock this store dates its writes by. A simulation of machines with skewed
    /// clocks or days passing sets it; nothing else does.
    pub fn set_write_clock_offset(&self, offset_ms: i64) -> Result<()> {
        let connection = self.connection.write();
        connection.execute("DELETE FROM temp.write_clock", [])?;
        connection.execute(
            "INSERT INTO temp.write_clock(offset_ms) VALUES (?1)",
            [offset_ms],
        )?;
        Ok(())
    }

    /// Date this store's writes at `at_unix_ms` until its clock is set again. Claim IDs hash
    /// the time they were written, so a simulation that sets this before each step writes the
    /// same claims however fast it runs. Nothing else sets it.
    pub fn set_write_clock_at(&self, at_unix_ms: u128) -> Result<()> {
        let connection = self.connection.write();
        connection.execute("DELETE FROM temp.write_clock", [])?;
        connection.execute(
            "INSERT INTO temp.write_clock(offset_ms, at_ms) VALUES (0, ?1)",
            [i64::try_from(at_unix_ms)?],
        )?;
        Ok(())
    }
}

pub fn terms_key(terms: &SealTerms) -> String {
    let mut digest = Sha256::new();
    digest.update(serde_json::to_vec(terms).expect("seal terms serialize"));
    hex::encode(digest.finalize())
}

/// The accepted time of a new batch or claim by `origin`: the clock, but never before the cut
/// of a checkpoint this node has sealed, and never before `origin`'s newest batch. A seal
/// promises the other participants that nothing this node writes afterwards is dated before its
/// cut. Folds read claims in canonical order, which starts with the accepted time, so a writer
/// whose clock stepped back would otherwise date its new claims before its older ones.
/// `temp.write_clock` shifts or fixes the clock for a simulation; see
/// `Store::set_write_clock_offset` and `Store::set_write_clock_at`.
pub fn write_time(connection: &Connection, origin: &str) -> Result<u128> {
    let floor: Option<i64> = connection
        .prepare_cached("SELECT MAX(cut_unix_ms) FROM checkpoints")?
        .query_row([], |row| row.get(0))?;
    let newest_own: Option<String> = connection
        .prepare_cached(
            "SELECT accepted_at_unix_ms FROM batches WHERE origin=?1
             ORDER BY replica_sequence DESC LIMIT 1",
        )?
        .query_row([origin], |row| row.get(0))
        .optional()?;
    let (offset, at): (i64, Option<i64>) = connection
        .prepare_cached("SELECT offset_ms, at_ms FROM temp.write_clock")
        .and_then(|mut statement| statement.query_row([], |row| Ok((row.get(0)?, row.get(1)?))))
        .unwrap_or((0, None));
    let now = match at {
        Some(at) => i128::from(at),
        None => i128::try_from(now_ms()).unwrap_or(i128::MAX) + i128::from(offset),
    };
    let now = u128::try_from(now.max(0)).unwrap_or(0);
    let floor = floor
        .and_then(|floor| u128::try_from(floor).ok())
        .unwrap_or(0);
    let newest_own = newest_own
        .and_then(|accepted| accepted.parse::<u128>().ok())
        .unwrap_or(0);
    Ok(now.max(floor).max(newest_own))
}
