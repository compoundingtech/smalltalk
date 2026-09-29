//! Agreement on checkpoints: which writers take part, the seal, the verification, and when a
//! checkpoint is stable. Nothing here deletes anything; a stable checkpoint is what a trim may
//! act on.
//!
//! Sections 2 and 3 of `doc/fleet/smalltalk/checkpoint-design`. Participants and stability are
//! pure functions of the claims a node holds, like the membership fold: every node that holds
//! the same claims reaches the same answer, whatever order they arrived in.

use super::checkpoint::{
    CheckpointProof, DropPlan, SealedIdentities, checkpoint_name, newest_due_cut, rules_digest,
};
use super::*;

pub const CHECKPOINT_SEALED: &str = "checkpoint.sealed";
pub const CHECKPOINT_VERIFIED: &str = "checkpoint.verified";
pub const CHECKPOINT_EXCUSED: &str = "checkpoint.excused";

/// The version of this agreement protocol, carried in every seal and verification.
pub const CHECKPOINT_PROTOCOL: u64 = 1;

const DAY_MS: u128 = 86_400_000;

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
    fn text(&self, name: &str) -> Option<&str> {
        self.fields.get(name).and_then(Value::as_str)
    }

    fn number(&self, name: &str) -> Option<u128> {
        self.fields
            .get(name)
            .and_then(Value::as_u64)
            .map(u128::from)
    }

    fn names(&self, name: &str) -> Option<BTreeSet<String>> {
        self.fields
            .get(name)?
            .as_array()?
            .iter()
            .map(|value| value.as_str().map(str::to_owned))
            .collect()
    }

    /// The cut, when the claim's subject names the same checkpoint as its field.
    fn cut(&self) -> Option<u128> {
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

    fn is_checkpoint_work(&self) -> bool {
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

fn seal_difference(ours: &SealTerms, theirs: &SealTerms) -> String {
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
fn checkpoint_actor(origin: &str) -> String {
    format!("agent/st3/checkpoint-{origin}")
}

fn attention_subject(checkpoint: &str) -> String {
    format!(
        "attention/checkpoint-{}",
        checkpoint.trim_start_matches("checkpoint/")
    )
}

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

    fn checkpoint_participants(
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
    fn record_checkpoint_state(
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
    fn append_checkpoint_claim(
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

    fn publish_seal(
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

    fn publish_verification(
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
        self.withdraw_checkpoint_attention(&claims, newest_stable, &mut actions)?;
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
                rules_digest: rules_digest(),
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
        self.request_checkpoint_attention(&claims, newest_stable, cut, context, &mut actions)?;
        Ok(actions)
    }

    fn verify_checkpoint(
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

    /// The first checkpoint after the newest stable one that is still not stable
    /// `CHECKPOINT_ATTENTION_AFTER_MS` after it became due. The participant that sorts first
    /// among those that sealed asks the person to bring the others back or excuse them.
    fn request_checkpoint_attention(
        &self,
        claims: &[CheckpointClaim],
        newest_stable: Option<u128>,
        cut: u128,
        context: &CheckpointContext,
        actions: &mut Vec<CheckpointAction>,
    ) -> Result<()> {
        let checkpoint = checkpoint_name(cut);
        let seals = newest_seals(claims, &checkpoint);
        let Some(terms) = seals.get(&self.origin) else {
            return Ok(());
        };
        let first_waiting = newest_stable.map_or_else(
            || {
                claims
                    .iter()
                    .filter_map(|claim| claim.seal_terms())
                    .map(|terms| terms.cut_unix_ms)
                    .min()
                    .unwrap_or(cut)
            },
            |stable| stable + DAY_MS,
        );
        let waiting_since = first_waiting + 2 * DAY_MS;
        if context.now_unix_ms < waiting_since + CHECKPOINT_ATTENTION_AFTER_MS {
            return Ok(());
        }
        if seals.keys().next() != Some(&self.origin) {
            return Ok(());
        }
        let waiting_for = terms
            .participants
            .iter()
            .filter(|writer| seals.get(*writer) != Some(terms))
            .cloned()
            .collect::<BTreeSet<_>>();
        if waiting_for.is_empty() {
            return Ok(());
        }
        let episode = checkpoint_name(first_waiting);
        let subject = attention_subject(&episode);
        if self.attention_request(&subject)?.is_some() {
            return Ok(());
        }
        let names = waiting_for.iter().cloned().collect::<Vec<_>>().join(", ");
        self.request_attention(
            &subject,
            &AttentionRequest {
                reviewer: context.reviewer.clone(),
                title: format!("Checkpoints are waiting for {names}"),
                reason: format!(
                    "No checkpoint has become stable since {episode} was due, because {names} \
                     has not sealed {checkpoint}. Bring it back, upgrade it, or run `st \
                     replication checkpoint excuse NAME --reason ...` for a machine that stays \
                     away; its writes still replicate when it returns. Remove it with `st fleet \
                     remove` only if its storage is gone."
                ),
                severity: "warning".into(),
                targets: vec![checkpoint.clone()],
                actor: checkpoint_actor(&self.origin),
                idempotency_key: format!("checkpoint-attention:{}:{episode}", self.origin),
            },
        )
        .map_err(|error| anyhow::anyhow!("{}: {}", error.code, error.message))?;
        actions.push(CheckpointAction::AttentionRequested {
            checkpoint,
            waiting_for,
        });
        Ok(())
    }

    /// Withdraw this node's attention requests about checkpoints once a checkpoint at or after
    /// the one they were about is stable.
    fn withdraw_checkpoint_attention(
        &self,
        claims: &[CheckpointClaim],
        newest_stable: Option<u128>,
        actions: &mut Vec<CheckpointAction>,
    ) -> Result<()> {
        let Some(newest_stable) = newest_stable else {
            return Ok(());
        };
        let cuts = claims
            .iter()
            .filter(|claim| claim.writer == self.origin)
            .filter_map(|claim| claim.seal_terms())
            .map(|terms| terms.cut_unix_ms)
            .filter(|cut| *cut <= newest_stable)
            .collect::<BTreeSet<_>>();
        for cut in cuts {
            let subject = attention_subject(&checkpoint_name(cut));
            let Some(request) = self.attention_request(&subject)? else {
                continue;
            };
            // Requests replicate; each node withdraws only its own.
            if request.status != "pending" || request.actor != checkpoint_actor(&self.origin) {
                continue;
            }
            self.withdraw_attention(
                &subject,
                &AttentionWithdrawRequest {
                    reason: format!("{} is stable", checkpoint_name(newest_stable)),
                    actor: checkpoint_actor(&self.origin),
                    idempotency_key: format!(
                        "checkpoint-attention-withdrawn:{}:{cut}",
                        self.origin
                    ),
                },
            )
            .map_err(|error| anyhow::anyhow!("{}: {}", error.code, error.message))?;
            actions.push(CheckpointAction::AttentionWithdrawn { attention: subject });
        }
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
                rules_digest: rules_digest(),
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

fn terms_key(terms: &SealTerms) -> String {
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
pub(super) fn write_time(connection: &Connection, origin: &str) -> Result<u128> {
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

#[cfg(test)]
mod tests {
    use super::*;

    const FLEET: &str = "fleet/test";
    const CUT: u128 = 20 * DAY_MS;

    fn claim(kind: &str, subject: &str, writer: &str, fields: Value) -> CheckpointClaim {
        CheckpointClaim {
            id: format!("{kind}:{subject}:{writer}:{fields}"),
            kind: kind.into(),
            subject: subject.into(),
            writer: writer.into(),
            actor: None,
            fields: fields.as_object().cloned().unwrap_or_default(),
        }
    }

    fn names(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    fn seal(writer: &str, participants: &[&str], digest: &str) -> CheckpointClaim {
        claim(
            CHECKPOINT_SEALED,
            &checkpoint_name(CUT),
            writer,
            json!({
                "cut_unix_ms": CUT, "participants": participants, "sealed_digest": digest,
                "sealed_count": 1, "rules_digest": "rules", "checkpoint_protocol": 1,
            }),
        )
    }

    fn verified(writer: &str, participants: &[&str], drop: &str) -> CheckpointClaim {
        claim(
            CHECKPOINT_VERIFIED,
            &checkpoint_name(CUT),
            writer,
            json!({
                "cut_unix_ms": CUT, "participants": participants, "sealed_digest": "sealed",
                "rules_digest": "rules", "drop_digest": drop, "dropped_envelopes": 1,
                "dropped_claims": 1, "retained_digest": "retained", "graph_digest": "graph",
                "reader_digest": "reader", "checkpoint_protocol": 1,
            }),
        )
    }

    fn excusal(writer: &str, actor: &str) -> CheckpointClaim {
        CheckpointClaim {
            actor: Some(actor.into()),
            ..claim(
                CHECKPOINT_EXCUSED,
                &format!("checkpoint-excusal/{writer}"),
                "alder",
                json!({"writer": writer, "reason": "gone"}),
            )
        }
    }

    #[test]
    fn participants_are_known_writers_less_leavers_and_excused_writers() {
        let known = names(&["alder", "birch", "cedar", "dogwood"]);
        let left = names(&["dogwood"]);
        // A writer named only by another node's seal is a participant here too.
        let claims = vec![seal("alder", &["alder", "birch", "cedar", "elm"], "d")];
        assert_eq!(
            participants(&known, &left, &claims),
            names(&["alder", "birch", "cedar", "elm"])
        );

        // A person's excusal takes a writer out; an agent's or the system's does nothing.
        let mut claims = claims;
        claims.push(excusal("cedar", "person/operator"));
        claims.push(excusal("birch", "agent/alder.worker"));
        claims.push(CheckpointClaim {
            actor: None,
            ..excusal("elm", "person/operator")
        });
        assert_eq!(
            participants(&known, &left, &claims),
            names(&["alder", "birch", "elm"])
        );
        // The excused writer's own next seal ends its excusal.
        claims.push(seal("cedar", &["alder", "birch", "cedar"], "d"));
        assert_eq!(excused_writers(&claims), BTreeSet::new());
        assert!(participants(&known, &left, &claims).contains("cedar"));
        // A removal is not a leave: a removed writer stays until it is excused.
        assert!(participants(&known, &BTreeSet::new(), &claims).contains("dogwood"));
    }

    #[test]
    fn a_certificate_needs_every_participant_with_identical_terms() {
        let all = ["alder", "birch", "cedar"];
        let complete = all.map(|writer| verified(writer, &all, "drop"));
        let checkpoint = checkpoint_name(CUT);
        assert_eq!(certificates(&complete, &checkpoint).len(), 1);
        let stable = stable_checkpoints(&complete);
        assert_eq!(stable.keys().copied().collect::<Vec<_>>(), [CUT]);

        // One participant missing.
        assert!(certificates(&complete[..2], &checkpoint).is_empty());
        // One digest different.
        let mut different = complete.to_vec();
        different[2] = verified("cedar", &all, "another drop");
        assert!(certificates(&different, &checkpoint).is_empty());
        // The participants disagree.
        let mut disagreeing = complete.to_vec();
        disagreeing[2] = verified("cedar", &["alder", "birch", "cedar", "dogwood"], "drop");
        assert!(certificates(&disagreeing, &checkpoint).is_empty());
        // A second verification from a node is ignored, before or after the certificate.
        let mut second = vec![complete[0].clone(), verified("alder", &all, "other")];
        second.extend_from_slice(&complete[1..]);
        assert_eq!(
            certificates(&second, &checkpoint),
            certificates(&complete, &checkpoint)
        );
        let mut late = complete.to_vec();
        late.push(verified("birch", &all, "other"));
        late.push(seal("alder", &all, "resealed"));
        assert_eq!(
            certificates(&late, &checkpoint),
            certificates(&complete, &checkpoint)
        );
        // A seal is not a verification.
        let seals = all.map(|writer| seal(writer, &all, "sealed"));
        assert!(certificates(&seals, &checkpoint).is_empty());
    }

    #[test]
    fn stability_does_not_depend_on_claim_order() {
        let all = ["alder", "birch", "cedar"];
        let mut claims = all.map(|writer| verified(writer, &all, "drop")).to_vec();
        claims.extend(all.map(|writer| seal(writer, &all, "sealed")));
        claims.push(excusal("dogwood", "person/operator"));
        let expected = stable_checkpoints(&claims);
        for rotation in 0..claims.len() {
            let mut rotated = claims.clone();
            rotated.rotate_left(rotation);
            assert_eq!(stable_checkpoints(&rotated), expected);
            rotated.reverse();
            assert_eq!(stable_checkpoints(&rotated), expected);
        }
    }

    #[test]
    fn both_sides_of_an_excused_partition_can_certify_and_a_node_adopts_both() {
        // People excused each side of a partition. Each side certified the same cut alone.
        let west = ["alder", "birch"];
        let east = ["cedar", "dogwood"];
        let mut claims = west.map(|writer| verified(writer, &west, "west")).to_vec();
        claims.extend(east.map(|writer| verified(writer, &east, "east")));
        let certified = certificates(&claims, &checkpoint_name(CUT));
        assert_eq!(certified.len(), 2);
        claims.reverse();
        assert_eq!(certificates(&claims, &checkpoint_name(CUT)), certified);
        // Every node applies the same one: here the smaller drop digest, since both sides
        // have two participants.
        let chosen = chosen_certificate(&certified).unwrap();
        assert_eq!(chosen.terms.drop_digest, "east");
        let mut reversed = certified.clone();
        reversed.reverse();
        assert_eq!(chosen_certificate(&reversed), Some(chosen));
        // A side with more participants wins whatever its digest.
        let larger = ["cedar", "dogwood", "elm"];
        let mut claims = west.map(|writer| verified(writer, &west, "west")).to_vec();
        claims.extend(larger.map(|writer| verified(writer, &larger, "zzz")));
        let certified = certificates(&claims, &checkpoint_name(CUT));
        assert_eq!(
            chosen_certificate(&certified).unwrap().terms.drop_digest,
            "zzz"
        );
        assert_eq!(chosen_certificate(&[]), None);
    }

    // Stores exchanging for real.

    fn sync(nodes: &[&Store]) {
        for node in nodes {
            node.bind_fleet(FLEET).unwrap();
        }
        for _ in 0..3 {
            for source in nodes {
                for target in nodes {
                    if source.origin == target.origin {
                        continue;
                    }
                    let exchange = source
                        .export_replication_exchange(
                            FLEET,
                            &target.replication_inventory().unwrap(),
                        )
                        .unwrap();
                    target
                        .receive_replication_exchange(&source.origin, FLEET, &exchange)
                        .unwrap();
                    target.validate_replication_backlog().unwrap();
                    target.project_replication_backlog().unwrap();
                }
            }
        }
    }

    fn observe(store: &Store, n: usize) {
        for index in 0..n {
            store
                .append_claim(&ClaimInput {
                    subject: format!("daemon/{}", store.origin),
                    kind: "daemon.diagnostic".into(),
                    actor: None,
                    fields: BTreeMap::from([
                        ("severity".into(), json!("warning")),
                        ("code".into(), json!("slow-request")),
                        ("reason".into(), json!(format!("slow {index}"))),
                    ]),
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: Some(format!(
                        "{}-slow-{index}-{}",
                        store.origin,
                        Uuid::now_v7().simple()
                    )),
                })
                .unwrap();
        }
    }

    /// A context two days after tomorrow's cut, so everything written now is before the cut.
    fn context(scratch: &Path, days_later: u128) -> CheckpointContext {
        CheckpointContext {
            now_unix_ms: now_ms() + (3 + days_later) * DAY_MS,
            configured_peers: Vec::new(),
            scratch: scratch.to_path_buf(),
            reviewer: "person/operator".into(),
        }
    }

    fn step(store: &Store, context: &CheckpointContext) -> Vec<CheckpointAction> {
        store.checkpoint_step(context).unwrap()
    }

    fn kinds(actions: &[CheckpointAction]) -> Vec<&'static str> {
        actions
            .iter()
            .map(|action| match action {
                CheckpointAction::Sealed { .. } => "sealed",
                CheckpointAction::Verified { .. } => "verified",
                CheckpointAction::ProofFailed { .. } => "proof-failed",
                CheckpointAction::AttentionRequested { .. } => "attention",
                CheckpointAction::AttentionWithdrawn { .. } => "withdrawn",
                CheckpointAction::Trimmed { .. } => "trimmed",
                CheckpointAction::ManifestNeeded { .. } => "manifest-needed",
                CheckpointAction::TrimGraphChanged { .. } => "graph-changed",
            })
            .collect()
    }

    fn stable_cuts(store: &Store) -> Vec<u128> {
        stable_checkpoints(&store.checkpoint_claims().unwrap())
            .into_keys()
            .collect()
    }

    #[test]
    fn nodes_seal_then_verify_once_every_participant_sealed_the_same_set() {
        let scratch = tempfile::tempdir().unwrap();
        let context = context(scratch.path(), 0);
        let cut = newest_due_cut(context.now_unix_ms);
        let [alder, birch] = ["alder", "birch"].map(|name| Store::open_memory(name).unwrap());
        observe(&alder, 4);
        observe(&birch, 3);
        sync(&[&alder, &birch]);

        assert_eq!(kinds(&step(&alder, &context)), ["sealed"]);
        // Nothing more until birch has sealed the same set.
        assert!(step(&alder, &context).is_empty());
        assert_eq!(kinds(&step(&birch, &context)), ["sealed"]);
        sync(&[&alder, &birch]);
        assert_eq!(kinds(&step(&alder, &context)), ["verified"]);
        assert!(stable_cuts(&alder).is_empty());
        assert_eq!(kinds(&step(&birch, &context)), ["verified"]);
        sync(&[&alder, &birch]);
        for node in [&alder, &birch] {
            assert_eq!(stable_cuts(node), [cut]);
            assert_eq!(kinds(&step(node, &context)), ["trimmed"]);
            assert!(step(node, &context).is_empty());
        }
        let status = alder.checkpoint_status(context.now_unix_ms, &[]).unwrap();
        let stable = status.newest_stable.unwrap();
        assert_eq!(stable.terms.participants, names(&["alder", "birch"]));
        assert!(status.pending.is_none());
        // Seals and verifications are dated at or after the cut, even though the clock was not.
        for claim in alder
            .claims_page(None, None, 0, None, false, 10_000)
            .unwrap()
            .claims
        {
            if claim.kind.starts_with("checkpoint.") {
                assert!(claim.accepted_at_unix_ms >= cut, "{claim:?}");
            } else {
                assert!(claim.accepted_at_unix_ms < cut, "{claim:?}");
            }
        }
    }

    #[test]
    fn a_late_envelope_before_the_cut_reseals_until_someone_verified() {
        let scratch = tempfile::tempdir().unwrap();
        let context = context(scratch.path(), 0);
        let [alder, birch, cedar] =
            ["alder", "birch", "cedar"].map(|name| Store::open_memory(name).unwrap());
        observe(&alder, 2);
        observe(&birch, 2);
        sync(&[&alder, &birch]);
        step(&alder, &context);
        step(&birch, &context);

        // An envelope from before the cut reaches alder late, from a writer it had not heard
        // of. Alder seals again, now with cedar as a participant.
        observe(&cedar, 1);
        sync(&[&alder, &cedar]);
        let actions = step(&alder, &context);
        let CheckpointAction::Sealed { participants, .. } = &actions[0] else {
            panic!("{actions:?}");
        };
        assert_eq!(participants, &names(&["alder", "birch", "cedar"]));
        sync(&[&alder, &birch, &cedar]);
        for node in [&birch, &cedar] {
            assert_eq!(kinds(&step(node, &context)), ["sealed"]);
        }
        sync(&[&alder, &birch, &cedar]);
        assert_eq!(kinds(&step(&alder, &context)), ["verified"]);

        // Once alder verified, a later envelope before the cut never makes it seal again.
        let dogwood = Store::open_memory("dogwood").unwrap();
        observe(&dogwood, 1);
        sync(&[&alder, &dogwood]);
        assert!(step(&alder, &context).is_empty());
        let status = alder.checkpoint_status(context.now_unix_ms, &[]).unwrap();
        assert!(status.pending.unwrap().unsealed.contains("dogwood"));
    }

    #[test]
    fn a_silent_participant_holds_everything_up_until_a_person_excuses_it() {
        let scratch = tempfile::tempdir().unwrap();
        let first = context(scratch.path(), 0);
        let [alder, birch, cedar] =
            ["alder", "birch", "cedar"].map(|name| Store::open_memory(name).unwrap());
        observe(&alder, 2);
        observe(&birch, 2);
        // Cedar is an old build, or away: it wrote once and never seals.
        observe(&cedar, 1);
        sync(&[&alder, &birch, &cedar]);
        step(&alder, &first);
        step(&birch, &first);
        sync(&[&alder, &birch]);
        for _ in 0..3 {
            assert!(!kinds(&step(&alder, &first)).contains(&"verified"));
            assert!(!kinds(&step(&birch, &first)).contains(&"verified"));
        }

        // Three days after it became due, the first participant to have sealed asks a person.
        let late = CheckpointContext {
            now_unix_ms: first.now_unix_ms + CHECKPOINT_ATTENTION_AFTER_MS,
            ..first.clone()
        };
        // The newest due checkpoint moved on; seal it first.
        step(&alder, &late);
        step(&birch, &late);
        sync(&[&alder, &birch]);
        let actions = step(&alder, &late);
        assert!(
            actions.iter().any(|action| matches!(
                action,
                CheckpointAction::AttentionRequested { waiting_for, .. }
                    if waiting_for == &names(&["cedar"])
            )),
            "{actions:?}"
        );
        assert!(!kinds(&step(&birch, &late)).contains(&"attention"));

        // An agent cannot excuse it.
        assert!(
            alder
                .excuse_checkpoint_writer(&CheckpointExcuseRequest {
                    writer: "cedar".into(),
                    reason: "away".into(),
                    actor: "agent/alder.worker".into(),
                })
                .is_err()
        );
        alder
            .excuse_checkpoint_writer(&CheckpointExcuseRequest {
                writer: "cedar".into(),
                reason: "the laptop is in a drawer".into(),
                actor: "person/operator".into(),
            })
            .unwrap();
        sync(&[&alder, &birch]);
        for node in [&alder, &birch] {
            assert_eq!(kinds(&step(node, &late)), ["sealed"]);
        }
        sync(&[&alder, &birch]);
        for node in [&alder, &birch] {
            assert_eq!(kinds(&step(node, &late)), ["verified"]);
        }
        sync(&[&alder, &birch]);
        let cut = newest_due_cut(late.now_unix_ms);
        assert_eq!(stable_cuts(&alder), [cut]);
        assert_eq!(kinds(&step(&alder, &late)), ["trimmed", "withdrawn"]);
        assert_eq!(kinds(&step(&birch, &late)), ["trimmed"]);

        // Cedar comes back. What it wrote while away replicates. It adopts the checkpoint it
        // missed, and its next seal ends its excusal, so the next checkpoint waits for it.
        observe(&cedar, 1);
        sync(&[&alder, &birch, &cedar]);
        let next = CheckpointContext {
            now_unix_ms: late.now_unix_ms + DAY_MS,
            ..late.clone()
        };
        assert_eq!(kinds(&step(&cedar, &next)), ["manifest-needed"]);
        let need = cedar.checkpoint_manifest_need().unwrap().unwrap();
        let manifest = alder
            .checkpoint_manifest(&need.checkpoint, need.cut_unix_ms)
            .unwrap();
        assert_eq!(
            kinds(&cedar.adopt_checkpoint(&manifest).unwrap()),
            ["trimmed"]
        );
        assert_eq!(claim_ids(&cedar), claim_ids(&alder));
        assert_eq!(kinds(&step(&cedar, &next)), ["sealed"]);
        sync(&[&alder, &birch, &cedar]);
        assert!(
            alder
                .checkpoint_status(next.now_unix_ms, &[])
                .unwrap()
                .excused
                .is_empty()
        );
        let actions = step(&alder, &next);
        let CheckpointAction::Sealed { participants, .. } = &actions[0] else {
            panic!("{actions:?}");
        };
        assert!(participants.contains("cedar"));
    }

    #[test]
    fn a_seal_dates_every_later_write_at_or_after_its_cut() {
        let scratch = tempfile::tempdir().unwrap();
        let context = context(scratch.path(), 0);
        let cut = newest_due_cut(context.now_unix_ms);
        let alder = Store::open_memory("alder").unwrap();
        observe(&alder, 1);
        step(&alder, &context);
        // The clock goes back three days.
        alder.set_write_clock_offset(-(3 * DAY_MS as i64)).unwrap();
        observe(&alder, 3);
        alder
            .put_document("doc/alder-notes", b"invented notes", &None, "alder-notes")
            .unwrap();
        let claims = alder
            .claims_page(None, None, 0, None, false, 10_000)
            .unwrap()
            .claims;
        let after_seal = claims
            .iter()
            .skip_while(|claim| claim.kind != CHECKPOINT_SEALED)
            .collect::<Vec<_>>();
        assert!(after_seal.len() > 3);
        for claim in after_seal {
            assert!(claim.accepted_at_unix_ms >= cut, "{claim:?}");
        }
        // A clock ahead of the cut is used as it is.
        alder.set_write_clock_offset(10 * DAY_MS as i64).unwrap();
        observe(&alder, 1);
        let newest = alder
            .claims_page(None, None, 0, None, false, 10_000)
            .unwrap()
            .claims;
        assert!(newest.last().unwrap().accepted_at_unix_ms > cut + 5 * DAY_MS);
    }

    /// Folds read a subject's claims in canonical order, which starts with the accepted time. A
    /// writer whose clock steps back must still date each new claim at or after its last one,
    /// or its new state would sort before its old state and every node would show the old.
    #[test]
    fn a_writer_never_dates_a_claim_before_its_own_newest() {
        let alder = Store::open_memory("alder").unwrap();
        alder.set_write_clock_offset(2 * DAY_MS as i64).unwrap();
        observe(&alder, 2);
        alder.set_write_clock_offset(0).unwrap();
        observe(&alder, 2);
        let claims = alder
            .claims_page(None, None, 0, None, false, 10_000)
            .unwrap()
            .claims;
        assert_eq!(claims.len(), 4);
        for pair in claims.windows(2) {
            assert!(pair[1].accepted_at_unix_ms >= pair[0].accepted_at_unix_ms);
        }
        let newest = alder
            .latest_claim(
                &format!("daemon/{}", alder.origin),
                Some("daemon.diagnostic"),
            )
            .unwrap()
            .unwrap();
        assert_eq!(newest.id, claims.last().unwrap().id);
    }

    /// A trim that would change the graph stops, records why, and seals nothing more until a
    /// person has looked. A trim that only bumps the graph generation compares the digests and
    /// goes on.
    #[test]
    fn a_trim_that_would_change_the_graph_waits_for_a_person() {
        let scratch = tempfile::tempdir().unwrap();
        let context = context(scratch.path(), 0);
        let [alder, birch] = ["alder", "birch"].map(|name| Store::open_memory(name).unwrap());
        stable_pair(&alder, &birch, &context);
        alder.set_trim_fault(Some(TrimFault::TouchGraph));
        assert_eq!(kinds(&step(&alder, &context)), ["trimmed"]);

        let (_, graph) = authority(&birch);
        let held = claim_ids(&birch);
        birch.set_trim_fault(Some(TrimFault::ChangeGraph));
        assert_eq!(kinds(&step(&birch, &context)), ["graph-changed"]);
        assert_eq!(authority(&birch).1, graph);
        assert!(claim_ids(&birch).is_superset(&held));
        let status = birch.checkpoint_status(context.now_unix_ms, &[]).unwrap();
        assert!(status.halted);
        assert_eq!(status.trimmed, None);
        assert!(
            birch
                .claims_for("daemon/birch", Some("daemon.diagnostic"))
                .unwrap()
                .iter()
                .any(|claim| claim.body["fields"]["code"] == "checkpoint-trim-graph-changed")
        );

        // Nothing more happens, even when the next checkpoint is due.
        let next = CheckpointContext {
            now_unix_ms: context.now_unix_ms + DAY_MS,
            ..context.clone()
        };
        observe(&birch, 1);
        assert!(step(&birch, &next).is_empty());
        assert!(
            birch
                .resume_checkpoints("agent/birch.worker", "looked")
                .is_err()
        );
        assert!(birch.resume_checkpoints("person/operator", " ").is_err());
        birch
            .resume_checkpoints("person/operator", "the change came from a test fault")
            .unwrap();
        assert!(
            !birch
                .checkpoint_status(next.now_unix_ms, &[])
                .unwrap()
                .halted
        );
        assert_eq!(kinds(&step(&birch, &next)), ["sealed"]);
    }

    fn authority(store: &Store) -> (String, String) {
        let status = store.replication_status(true, Some(FLEET), &[]).unwrap();
        (status.authority_digest, status.graph_digest)
    }

    fn claim_ids(store: &Store) -> BTreeSet<String> {
        store
            .claims_page(None, None, 0, None, false, 100_000)
            .unwrap()
            .claims
            .into_iter()
            .map(|claim| claim.id)
            .collect()
    }

    /// Two nodes agree on a checkpoint until it is stable, without trimming it.
    fn stable_pair(alder: &Store, birch: &Store, context: &CheckpointContext) -> String {
        observe(alder, 8);
        observe(birch, 5);
        sync(&[alder, birch]);
        for round in ["sealed", "verified"] {
            for node in [alder, birch] {
                assert_eq!(kinds(&step(node, context)), [round]);
            }
            sync(&[alder, birch]);
        }
        checkpoint_name(newest_due_cut(context.now_unix_ms))
    }

    #[test]
    fn participants_trim_the_stable_checkpoint_and_keep_every_identity() {
        let scratch = tempfile::tempdir().unwrap();
        let context = context(scratch.path(), 0);
        let cut = newest_due_cut(context.now_unix_ms);
        let [alder, birch] = ["alder", "birch"].map(|name| Store::open_memory(name).unwrap());
        let checkpoint = stable_pair(&alder, &birch, &context);
        let before = [&alder, &birch].map(authority);
        let held = claim_ids(&alder);
        let index = alder.index().unwrap();
        for node in [&alder, &birch] {
            assert_eq!(kinds(&step(node, &context)), ["trimmed"]);
            assert!(step(node, &context).is_empty());
        }
        // Every identity stays in the inventory, so the authority digest does not move, and
        // the graph is the same.
        assert_eq!([&alder, &birch].map(authority), before);
        assert_eq!(authority(&alder), authority(&birch));
        let kept = claim_ids(&alder);
        assert!(kept.len() < held.len());
        assert_eq!(kept, claim_ids(&birch));
        let manifests =
            [&alder, &birch].map(|node| node.checkpoint_manifest(&checkpoint, cut).unwrap());
        assert!(!manifests[0].claims.is_empty());
        assert_eq!(manifests[0], manifests[1]);
        for claim in &manifests[0].claims {
            assert!(held.contains(&claim.id) && !kept.contains(&claim.id));
        }
        // Snapshots taken before the trim expire.
        assert!(alder.index().unwrap() > index);
        let status = alder.checkpoint_status(context.now_unix_ms, &[]).unwrap();
        assert_eq!(status.trimmed.as_deref(), Some(checkpoint.as_str()));

        // Both keep replicating new writes.
        observe(&alder, 2);
        observe(&birch, 2);
        sync(&[&alder, &birch]);
        assert_eq!(authority(&alder), authority(&birch));
        assert_eq!(claim_ids(&alder), claim_ids(&birch));
    }

    #[test]
    fn a_trim_that_stops_anywhere_finishes_the_same_after_a_restart() {
        for fault in [
            TrimFault::AfterTombstones,
            TrimFault::AfterChunk(1),
            TrimFault::AfterChunk(2),
            TrimFault::BeforeFinish,
        ] {
            let scratch = tempfile::tempdir().unwrap();
            let context = context(scratch.path(), 0);
            let path = scratch.path().join("alder.sqlite3");
            let alder = Store::open(&path, "alder").unwrap();
            let birch = Store::open_memory("birch").unwrap();
            stable_pair(&alder, &birch, &context);
            assert_eq!(kinds(&step(&birch, &context)), ["trimmed"]);
            alder.set_trim_chunk_envelopes(2);
            alder.set_trim_fault(Some(fault));
            assert!(alder.checkpoint_step(&context).is_err(), "{fault:?}");
            drop(alder);

            let alder = Store::open(&path, "alder").unwrap();
            let actions = step(&alder, &context);
            assert_eq!(kinds(&actions), ["trimmed"], "{fault:?}");
            assert!(step(&alder, &context).is_empty());
            assert_eq!(claim_ids(&alder), claim_ids(&birch), "{fault:?}");
            assert_eq!(authority(&alder), authority(&birch), "{fault:?}");
            let cut = newest_due_cut(context.now_unix_ms);
            let checkpoint = checkpoint_name(cut);
            assert_eq!(
                alder.checkpoint_manifest(&checkpoint, cut).unwrap(),
                birch.checkpoint_manifest(&checkpoint, cut).unwrap(),
                "{fault:?}"
            );
        }
    }

    #[test]
    fn a_node_that_did_not_take_part_adopts_the_manifest_and_nothing_else() {
        let scratch = tempfile::tempdir().unwrap();
        let context = context(scratch.path(), 0);
        let cut = newest_due_cut(context.now_unix_ms);
        let [alder, birch] = ["alder", "birch"].map(|name| Store::open_memory(name).unwrap());
        let checkpoint = stable_pair(&alder, &birch, &context);
        for node in [&alder, &birch] {
            step(node, &context);
        }
        // A new node gets the kept envelopes, never the dropped ones, and the checkpoint claims.
        let cedar = Store::open_memory("cedar").unwrap();
        sync(&[&alder, &cedar]);
        assert_ne!(authority(&cedar).0, authority(&alder).0);
        assert_eq!(kinds(&step(&cedar, &context)), ["manifest-needed"]);
        assert_eq!(
            cedar
                .checkpoint_manifest_need()
                .unwrap()
                .map(|need| need.checkpoint),
            Some(checkpoint.clone())
        );
        let manifest = alder.checkpoint_manifest(&checkpoint, cut).unwrap();

        // A changed tombstone is refused before anything is stored.
        let mut changed = manifest.clone();
        changed.claims[0]
            .predecessors
            .push("an-invented-claim".into());
        assert_eq!(
            cedar.adopt_checkpoint(&changed).unwrap_err().code,
            "checkpoint-manifest-mismatch"
        );
        assert_eq!(cedar.checkpointed_envelopes().unwrap(), 0);

        assert_eq!(
            kinds(&cedar.adopt_checkpoint(&manifest).unwrap()),
            ["trimmed"]
        );
        assert_eq!(cedar.checkpoint_manifest_need().unwrap(), None);
        assert_eq!(authority(&cedar), authority(&alder));
        assert_eq!(claim_ids(&cedar), claim_ids(&alder));
        assert_eq!(
            cedar.checkpoint_manifest(&checkpoint, cut).unwrap(),
            manifest
        );
        assert!(step(&cedar, &context).is_empty());
    }

    fn excuse(store: &Store, writer: &str) {
        store
            .excuse_checkpoint_writer(&CheckpointExcuseRequest {
                writer: writer.into(),
                reason: "cut off by a partition".into(),
                actor: "person/operator".into(),
            })
            .unwrap();
    }

    /// People on each side of a partition excuse the other side, and each side certifies and
    /// trims the same cut alone. Once the partition heals, every node applies the same one of
    /// the two certificates, and they end with identical tombstones, inventories and graphs,
    /// and trim the next checkpoint together.
    #[test]
    fn both_sides_of_an_excused_partition_converge_on_one_certificate() {
        let scratch = tempfile::tempdir().unwrap();
        let context = context(scratch.path(), 0);
        let cut = newest_due_cut(context.now_unix_ms);
        let checkpoint = checkpoint_name(cut);
        let [alder, birch, cedar, dogwood] =
            ["alder", "birch", "cedar", "dogwood"].map(|name| Store::open_memory(name).unwrap());
        let all = [&alder, &birch, &cedar, &dogwood];
        for node in all {
            observe(node, 4);
        }
        sync(&all);
        let west = [&alder, &birch];
        let east = [&cedar, &dogwood];
        for (side, others) in [(west, ["cedar", "dogwood"]), (east, ["alder", "birch"])] {
            for node in side {
                observe(node, 3);
            }
            for writer in others {
                excuse(side[0], writer);
            }
            sync(&side);
            for round in ["sealed", "verified", "trimmed"] {
                for node in side {
                    assert_eq!(kinds(&step(node, &context)), [round], "{}", node.origin);
                }
                sync(&side);
            }
        }
        let [west_certificate, east_certificate] =
            [&alder, &cedar].map(|node| node.trimmed_checkpoint().unwrap().unwrap());
        assert_ne!(west_certificate.drop_digest, east_certificate.drop_digest);

        // The partition heals.
        sync(&all);
        let certified = certificates(&alder.checkpoint_claims().unwrap(), &checkpoint);
        assert_eq!(certified.len(), 2);
        let chosen = chosen_certificate(&certified).unwrap().clone();
        let (kept, switching) = if chosen.terms.drop_digest == west_certificate.drop_digest {
            (west, east)
        } else {
            (east, west)
        };
        for node in kept {
            assert_eq!(node.checkpoint_manifest_need().unwrap(), None);
            assert!(!kinds(&step(node, &context)).contains(&"manifest-needed"));
        }
        let manifest = kept[0].checkpoint_manifest(&checkpoint, cut).unwrap();
        for node in switching {
            assert_eq!(kinds(&step(node, &context)), ["manifest-needed"]);
            let need = node.checkpoint_manifest_need().unwrap().unwrap();
            assert_eq!(need.drop_digest, chosen.terms.drop_digest);
            // The other side's manifest does not verify against the chosen certificate.
            let own = node.checkpoint_manifest(&checkpoint, cut).unwrap();
            assert!(node.adopt_checkpoint(&own).is_err());
            assert_eq!(
                kinds(&node.adopt_checkpoint(&manifest).unwrap()),
                ["trimmed"]
            );
            assert_eq!(
                node.checkpoint_manifest(&checkpoint, cut).unwrap(),
                manifest
            );
        }
        sync(&all);
        for node in all {
            assert_eq!(authority(node), authority(&alder), "{}", node.origin);
            assert_eq!(claim_ids(node), claim_ids(&alder), "{}", node.origin);
            assert_eq!(
                node.checkpoint_manifest(&checkpoint, cut).unwrap(),
                manifest,
                "{}",
                node.origin
            );
            assert_eq!(
                node.trimmed_checkpoint().unwrap().unwrap().drop_digest,
                chosen.terms.drop_digest
            );
            assert!(!kinds(&step(node, &context)).contains(&"manifest-needed"));
        }

        // Every writer seals the next checkpoint, which ends its excusal, and all four trim it
        // together.
        let next = CheckpointContext {
            now_unix_ms: context.now_unix_ms + DAY_MS,
            ..context.clone()
        };
        for node in all {
            observe(node, 2);
        }
        sync(&all);
        for round in ["sealed", "verified", "trimmed"] {
            for node in all {
                assert!(
                    kinds(&step(node, &next)).contains(&round),
                    "{} did not reach {round}",
                    node.origin
                );
            }
            sync(&all);
        }
        for node in all {
            assert_eq!(authority(node), authority(&alder), "{}", node.origin);
            assert_eq!(claim_ids(node), claim_ids(&alder), "{}", node.origin);
        }
        // The older checkpoint's manifest lacks the newer drops, so no node adopts it now.
        assert_eq!(
            alder.adopt_checkpoint(&manifest).unwrap_err().code,
            "checkpoint-superseded"
        );
    }

    #[test]
    fn a_node_that_left_the_fleet_is_not_waited_for() {
        let known = names(&["alder", "birch"]);
        let left = names(&["birch"]);
        assert_eq!(participants(&known, &left, &[]), names(&["alder"]));
    }
}
