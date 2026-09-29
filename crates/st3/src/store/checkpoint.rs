//! Checkpoints: which replicated claims before a cut a stable checkpoint may drop, and the proof
//! that dropping them changes neither the graph nor what any reader returns.
//!
//! `doc/fleet/smalltalk/checkpoint-design` is the design. Each rule keeps, within a slot, the
//! claims a reader's answer depends on. It drops a claim only when a later kept claim of the same
//! slot replaces it for every fold that reads the kind (the claim's witness). The planner is a
//! pure function of the claims before the cut, in canonical order, so every node that holds the
//! same claims drops the same ones.

use super::*;

mod tombstones;

pub use tombstones::{
    CHECKPOINT_MANIFEST_PAGE_LIMIT, CheckpointManifest, CheckpointManifestCursor,
    CheckpointManifestPage, CheckpointManifestRequest, verify_checkpoint_manifest,
};
pub(crate) use tombstones::{
    checkpointed_operation, checkpointed_operations, claim_or_tombstone_exists, claim_tombstoned,
    envelope_tombstoned,
};

/// The rule engine's version. It is part of the rules digest, so nodes agree on a checkpoint only
/// when they run the same rules.
pub const RULES_VERSION: u32 = 1;

const DAY_MS: u128 = 86_400_000;

/// Kinds that are now local observations are dropped only when they are dated at least five days
/// before the cut, so they are seven days old when the checkpoint is due. That matches the local
/// observation log's default retention.
const LOCAL_KIND_MIN_AGE_MS: u128 = 5 * DAY_MS;

/// The optional fields `current_harness_at` folds newest first from a harness's observations.
const HARNESS_OPTIONAL_FIELDS: [&str; 7] = [
    "driver",
    "transport",
    "reason",
    "blocked_on",
    "ask",
    "input_buffer",
    "exit",
];

/// The claims that close a subscription's mission request, as `pending_subscription_mission_requests`
/// reads them.
const REQUEST_CLOSERS: [&str; 3] = [
    "subscription.mission-started",
    "subscription.mission-failed",
    "subscription.mission-request-cancelled",
];

/// A canonical description of every rule. The rules digest hashes it with `RULES_VERSION`.
const RULES_DESCRIPTION: &str = "\
harness.observed slot=subject,incarnation_id keep=first,first-ready,first-ready-not-provider-auth,newest,newest-not-working,every-working-after,newest-carrier-of-each-optional-field
harness.timeline slot=subject,incarnation_id keep=newest min-age-before-cut=5d
loop.state slot=subject keep=first-and-last-of-each-run-of-status-and-round,first-with-items
subscription.mission-deferred slot=subject,request keep=all-while-open,newest
observer.observed slot=subject keep=newest,newest-carrier-of-each-field
daemon.diagnostic slot=subject,code keep=newest,newest-carrier-of-each-field
transport.observed slot=subject,origin keep=newest,newest-carrier-of-each-field
runtime.action.requested actor=null slot=subject,action,incarnation_id,operation_status keep=newest min-age-before-cut=5d
runtime.action.succeeded actor=null slot=subject,action,incarnation_id,operation_status keep=newest min-age-before-cut=5d
runtime.action.failed actor=null slot=subject,action,incarnation_id,operation_status keep=newest min-age-before-cut=5d
runtime.action.deadline-reached actor=null slot=subject,action,incarnation_id,operation_status keep=newest min-age-before-cut=5d
render.applied slot=subject keep=newest min-age-before-cut=5d
runtime.readiness-deadline-reached slot=subject keep=newest min-age-before-cut=5d
guards=person-actor,once-cardinality,record-not-valid,repair-replacement,projection-reference,claim-in-two-envelopes,cited-as-evidence,shared-operation,writer-newest-envelope,whole-envelope
witness=every-field-set-again-by-a-later-kept-claim-of-the-slot
carriers=every-rule-but-loop.state-keeps-the-newest-carrier-of-each-field";

/// The digest of the rules this build applies.
pub fn rules_digest() -> String {
    let mut digest = Sha256::new();
    digest.update(b"st3-checkpoint-rules-v1\0");
    digest.update(RULES_VERSION.to_be_bytes());
    digest.update(RULES_DESCRIPTION.as_bytes());
    hex::encode(digest.finalize())
}

/// The name of the checkpoint whose cut is `cut_unix_ms`, for example `checkpoint/2026-09-27`.
pub fn checkpoint_name(cut_unix_ms: u128) -> String {
    let day = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(
        i64::try_from(cut_unix_ms).unwrap_or(i64::MAX),
    )
    .map_or_else(
        || cut_unix_ms.to_string(),
        |time| time.format("%Y-%m-%d").to_string(),
    );
    format!("checkpoint/{day}")
}

/// The cut of the checkpoint named for a UTC day, given as `YYYY-MM-DD` or `checkpoint/YYYY-MM-DD`.
pub fn checkpoint_cut(day: &str) -> Result<u128, St3Error> {
    let day = day.strip_prefix("checkpoint/").unwrap_or(day);
    let date = chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d").map_err(|_| {
        St3Error::new(
            "invalid-checkpoint",
            format!("`{day}` is not a UTC day such as 2026-09-27"),
        )
    })?;
    let start = date
        .and_hms_opt(0, 0, 0)
        .map(|time| time.and_utc().timestamp_millis())
        .and_then(|millis| u128::try_from(millis).ok())
        .ok_or_else(|| St3Error::new("invalid-checkpoint", format!("`{day}` is out of range")))?;
    Ok(start)
}

/// The cut of the newest checkpoint that is due at `now_unix_ms`: checkpoint `D` becomes due at
/// the start of day `D+2`.
pub fn newest_due_cut(now_unix_ms: u128) -> u128 {
    (now_unix_ms / DAY_MS).saturating_sub(2) * DAY_MS
}

/// A dry run of one checkpoint on this node: what it would drop, and the proof on a copy.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointPlanView {
    pub checkpoint: String,
    pub cut_unix_ms: u128,
    pub rules_digest: String,
    pub sealed_envelopes: usize,
    pub sealed_claims: usize,
    pub sealed_digest: String,
    pub dropped_envelopes: usize,
    pub dropped_claims: usize,
    pub by_kind: BTreeMap<String, DropCount>,
    pub drop_digest: String,
    pub retained_digest: String,
    pub proof: CheckpointProof,
}

/// Which checkpoint a dry run plans. Without a day, the newest due one.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CheckpointPlanRequest {
    #[serde(default)]
    pub day: Option<String>,
}

/// The identity of one envelope.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct EnvelopeKey {
    pub writer: String,
    pub sequence: u64,
    pub envelope_hash: String,
}

/// A dropped envelope. A node keeps it in place of the envelope, so inventories and the
/// authority digest do not change and no peer sends the envelope back.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct EnvelopeTombstone {
    pub writer: String,
    pub sequence: u64,
    pub envelope_hash: String,
    pub accepted_at_unix_ms: u128,
}

/// A dropped claim. It keeps what evidence checks, ancestry walks and idempotent retries read.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ClaimTombstone {
    pub id: String,
    pub writer: String,
    pub sequence: u64,
    pub envelope_hash: String,
    pub subject: String,
    pub kind: String,
    pub actor: Option<String>,
    pub predecessors: Vec<String>,
    pub operation_id: Option<String>,
    pub request_digest: Option<String>,
    pub accepted_at_unix_ms: u128,
}

/// One claim in the sealed set, with what the planner needs to decide about it.
#[derive(Clone, Debug)]
pub struct SealedClaim {
    pub claim: ClaimRecord,
    pub envelope: EnvelopeKey,
    /// The record admitted this claim as valid.
    pub valid: bool,
    /// A projection row or a repair names this claim, so it stays.
    pub protected: bool,
}

/// One envelope in the sealed set.
#[derive(Clone, Debug)]
pub struct SealedEnvelope {
    pub key: EnvelopeKey,
    pub accepted_at_unix_ms: u128,
    /// Every record the envelope holds, admitted or not.
    pub records: usize,
}

/// The envelopes before a cut and their admitted claims, in canonical order.
#[derive(Clone, Debug, Default)]
pub struct SealedSet {
    pub cut_unix_ms: u128,
    pub envelopes: Vec<SealedEnvelope>,
    pub claims: Vec<SealedClaim>,
}

/// What a checkpoint drops from a sealed set.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DropPlan {
    pub cut_unix_ms: u128,
    pub rules_digest: String,
    pub sealed_envelopes: usize,
    pub sealed_claims: usize,
    pub sealed_digest: String,
    pub envelopes: Vec<EnvelopeTombstone>,
    pub claims: Vec<ClaimTombstone>,
    pub by_kind: BTreeMap<String, DropCount>,
    pub drop_digest: String,
    pub retained_digest: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DropCount {
    pub sealed: usize,
    pub dropped: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Rule {
    Newest,
    NewestAged,
    HarnessObserved,
    LoopState,
    Deferral,
}

fn fields(claim: &ClaimRecord) -> Option<&serde_json::Map<String, Value>> {
    claim.body.get("fields").and_then(Value::as_object)
}

fn field_text(claim: &ClaimRecord, name: &str) -> String {
    match fields(claim).and_then(|fields| fields.get(name)) {
        Some(Value::String(text)) => text.clone(),
        Some(value) => value.to_string(),
        None => String::new(),
    }
}

fn field_str<'a>(claim: &'a ClaimRecord, name: &str) -> Option<&'a str> {
    fields(claim)
        .and_then(|fields| fields.get(name))
        .and_then(Value::as_str)
}

/// The rule and slot of a claim, or `None` when no rule may drop it.
fn slot_of(claim: &ClaimRecord) -> Option<(Rule, Vec<String>)> {
    let subject = claim.subject.clone();
    let kind = claim.kind.clone();
    let slot = |extra: &[&str]| {
        let mut slot = vec![subject.clone(), kind.clone()];
        slot.extend(extra.iter().map(|name| field_text(claim, name)));
        slot
    };
    match claim.kind.as_str() {
        // A legacy observation without an incarnation falls back to store index comparisons in
        // `current_harness_at`, so it stays.
        "harness.observed" => field_str(claim, "incarnation_id")
            .map(|_| (Rule::HarnessObserved, slot(&["incarnation_id"]))),
        "harness.timeline" => Some((Rule::NewestAged, slot(&["incarnation_id"]))),
        "loop.state" => Some((Rule::LoopState, slot(&[]))),
        "subscription.mission-deferred" => Some((Rule::Deferral, slot(&["request"]))),
        "observer.observed" => Some((Rule::Newest, slot(&[]))),
        "daemon.diagnostic" => Some((Rule::Newest, slot(&["code"]))),
        "transport.observed" => {
            let mut slot = slot(&[]);
            slot.push(claim.origin.clone());
            Some((Rule::Newest, slot))
        }
        // A person's signal names its requester and replicates with its result, so only the
        // reconciler's own records are dropped.
        "runtime.action.requested"
        | "runtime.action.succeeded"
        | "runtime.action.failed"
        | "runtime.action.deadline-reached"
            if claim.actor.is_none() =>
        {
            Some((
                Rule::NewestAged,
                slot(&["action", "incarnation_id", "operation_status"]),
            ))
        }
        "render.applied" | "runtime.readiness-deadline-reached" => {
            Some((Rule::NewestAged, slot(&[])))
        }
        _ => None,
    }
}

/// Whether every field `claim` sets is set again by a later kept claim of its slot, whose field
/// names are `later`. Folds read these kinds last writer wins, field by field, so the later
/// claims replace it whatever arrives in between. A state transition clears every field of its
/// kind, so any later claim of the kind replaces one that sets only schema fields.
fn witnessed(claim: &ClaimRecord, later: &BTreeSet<String>, later_claims: usize) -> bool {
    let own = fields(claim).map(|fields| fields.keys().cloned().collect::<Vec<_>>());
    let own = own.unwrap_or_default();
    if later_claims > 0
        && let Some(spec) = st3_schema::registry().claim(&claim.kind)
        && spec.cardinality == st3_schema::Cardinality::StateTransition
        && own.iter().all(|name| spec.fields.contains_key(name))
    {
        return true;
    }
    later_claims > 0 && own.iter().all(|name| later.contains(name))
}

/// The newest claim of a slot carrying each field name, so every field keeps its last value.
fn field_carriers(claims: &[&ClaimRecord]) -> BTreeSet<usize> {
    let mut seen = BTreeSet::new();
    let mut keep = BTreeSet::new();
    for (position, claim) in claims.iter().enumerate().rev() {
        for name in fields(claim).into_iter().flat_map(|fields| fields.keys()) {
            if seen.insert(name.clone()) {
                keep.insert(position);
            }
        }
    }
    keep
}

fn harness_keep(claims: &[&ClaimRecord]) -> BTreeSet<usize> {
    let state = |claim: &ClaimRecord| field_str(claim, "state").map(str::to_owned);
    let ready =
        |claim: &ClaimRecord| matches!(state(claim).as_deref(), Some("ready" | "working" | "idle"));
    let mut keep = BTreeSet::new();
    keep.insert(0);
    keep.insert(claims.len() - 1);
    // `park_unready_crash_loop` asks whether the incarnation was ever ready; `harness_was_ready`
    // asks the same without a provider-login reason.
    if let Some(position) = claims.iter().position(|claim| ready(claim)) {
        keep.insert(position);
    }
    if let Some(position) = claims
        .iter()
        .position(|claim| ready(claim) && field_str(claim, "reason") != Some("providerAuth"))
    {
        keep.insert(position);
    }
    // `agent_working_since`: the first `working` after the last other state. A late
    // observation of another state can land anywhere after the last one kept here, and the
    // answer is then the first `working` after it, so every `working` after the last other
    // state stays. Earlier ones can never be the answer again.
    let last_other = claims
        .iter()
        .rposition(|claim| state(claim).is_some_and(|state| state != "working"));
    if let Some(position) = last_other {
        keep.insert(position);
    }
    let after = last_other.map_or(0, |position| position + 1);
    for (offset, claim) in claims[after..].iter().enumerate() {
        if state(claim).as_deref() == Some("working") {
            keep.insert(after + offset);
        }
    }
    // `current_harness_at` takes each optional field from the newest observation carrying it.
    for name in HARNESS_OPTIONAL_FIELDS {
        if let Some(position) = claims
            .iter()
            .rposition(|claim| fields(claim).is_some_and(|fields| fields.contains_key(name)))
        {
            keep.insert(position);
        }
    }
    keep
}

fn loop_keep(claims: &[&ClaimRecord]) -> BTreeSet<usize> {
    let run_key = |claim: &ClaimRecord| (field_text(claim, "status"), field_text(claim, "round"));
    let mut keep = BTreeSet::new();
    for (position, claim) in claims.iter().enumerate() {
        let key = run_key(claim);
        let starts = position == 0 || run_key(claims[position - 1]) != key;
        let ends = position + 1 == claims.len() || run_key(claims[position + 1]) != key;
        if starts || ends {
            keep.insert(position);
        }
    }
    // `evaluate_for_each_loop` reads the first state that carries the loop's items.
    if let Some(position) = claims
        .iter()
        .position(|claim| fields(claim).is_some_and(|fields| fields.contains_key("items")))
    {
        keep.insert(position);
    }
    keep
}

type TimingEvent = (String, Value, u128);

fn timing_event(claim: &ClaimRecord) -> TimingEvent {
    (
        claim.kind.clone(),
        claim.body.clone(),
        claim.accepted_at_unix_ms,
    )
}

fn timing_answers(events: &[TimingEvent], attempt: u32, cut: u128) -> Vec<(Option<u128>, u128)> {
    [cut, u128::MAX]
        .into_iter()
        .flat_map(|snapshot| {
            [true, false]
                .into_iter()
                .map(move |active| fold_step_timing(events, attempt, snapshot, active))
        })
        .collect()
}

/// Decide what a checkpoint drops from `sealed`. See the module documentation and invariants
/// D1 to D6 of the design.
pub fn plan_drops(sealed: &SealedSet) -> DropPlan {
    let cut = sealed.cut_unix_ms;
    let claims = &sealed.claims;
    let mut dropped = vec![false; claims.len()];

    // A claim can be held in more than one envelope, when a writer's legacy and current
    // envelope hashes both reached this node. Each claim takes part in the rules once, by its
    // first occurrence, and a claim held twice always stays.
    let mut occurrences: BTreeMap<&str, usize> = BTreeMap::new();
    let mut first = Vec::new();
    for (index, sealed_claim) in claims.iter().enumerate() {
        let count = occurrences
            .entry(sealed_claim.claim.id.as_str())
            .or_default();
        *count += 1;
        if *count == 1 {
            first.push(index);
        }
    }

    // Slots, each in canonical order.
    let mut slots: BTreeMap<(Rule, Vec<String>), Vec<usize>> = BTreeMap::new();
    for index in first.iter().copied() {
        if let Some(key) = slot_of(&claims[index].claim) {
            slots.entry(key).or_default().push(index);
        }
    }
    let closed_requests = claims
        .iter()
        .filter(|sealed_claim| REQUEST_CLOSERS.contains(&sealed_claim.claim.kind.as_str()))
        .filter_map(|sealed_claim| {
            field_str(&sealed_claim.claim, "request")
                .map(|request| (sealed_claim.claim.subject.clone(), request.to_owned()))
        })
        .collect::<BTreeSet<_>>();
    for ((rule, slot), members) in &slots {
        let slot_claims = members
            .iter()
            .map(|index| &claims[*index].claim)
            .collect::<Vec<_>>();
        let newest = BTreeSet::from([members.len() - 1]);
        let keep = match rule {
            Rule::Newest | Rule::NewestAged => newest,
            Rule::HarnessObserved => harness_keep(&slot_claims),
            Rule::LoopState => loop_keep(&slot_claims),
            Rule::Deferral => {
                let request = slot.last().cloned().unwrap_or_default();
                if closed_requests.contains(&(slot[0].clone(), request)) {
                    newest
                } else {
                    (0..members.len()).collect()
                }
            }
        };
        let mut keep = keep;
        if *rule != Rule::LoopState {
            keep.extend(field_carriers(&slot_claims));
        }
        for (position, index) in members.iter().enumerate() {
            let old_enough = *rule != Rule::NewestAged
                || claims[*index].claim.accepted_at_unix_ms
                    < cut.saturating_sub(LOCAL_KIND_MIN_AGE_MS);
            if !keep.contains(&position) && old_enough {
                dropped[*index] = true;
            }
        }
    }

    // D4: guards.
    let cited = claims
        .iter()
        .flat_map(|sealed_claim| {
            sealed_claim
                .claim
                .body
                .get("evidence")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
        })
        .collect::<BTreeSet<_>>();
    let mut newest_envelope: BTreeMap<&str, u64> = BTreeMap::new();
    for envelope in &sealed.envelopes {
        let newest = newest_envelope
            .entry(envelope.key.writer.as_str())
            .or_default();
        *newest = (*newest).max(envelope.key.sequence);
    }
    for (index, sealed_claim) in claims.iter().enumerate() {
        let claim = &sealed_claim.claim;
        let cardinality = st3_schema::registry()
            .claim(&claim.kind)
            .map(|spec| spec.cardinality.clone());
        let guarded = claim
            .actor
            .as_deref()
            .is_some_and(|actor| actor.starts_with("person/"))
            || !matches!(
                cardinality,
                Some(st3_schema::Cardinality::Append | st3_schema::Cardinality::StateTransition)
            )
            || !sealed_claim.valid
            || sealed_claim.protected
            || occurrences[claim.id.as_str()] > 1
            || cited.contains(claim.id.as_str())
            || newest_envelope.get(sealed_claim.envelope.writer.as_str())
                == Some(&sealed_claim.envelope.sequence);
        if guarded {
            dropped[index] = false;
        }
    }

    // D3, shared operations and whole envelopes, until nothing changes. Each step only keeps
    // more, so the loop ends, and every witness is a kept claim.
    let mut operations: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    let mut envelope_claims: BTreeMap<&EnvelopeKey, Vec<usize>> = BTreeMap::new();
    for (index, sealed_claim) in claims.iter().enumerate() {
        if let Some(operation) = sealed_claim.claim.operation_id.as_deref() {
            operations.entry(operation).or_default().push(index);
        }
        envelope_claims
            .entry(&sealed_claim.envelope)
            .or_default()
            .push(index);
    }
    let records = sealed
        .envelopes
        .iter()
        .map(|envelope| (&envelope.key, envelope.records))
        .collect::<BTreeMap<_, _>>();
    loop {
        let mut changed = false;
        // Newest first, so a claim kept back here can witness the ones before it.
        for members in slots.values() {
            let mut later = BTreeSet::new();
            let mut later_claims = 0;
            for index in members.iter().rev() {
                let claim = &claims[*index].claim;
                if dropped[*index] && !witnessed(claim, &later, later_claims) {
                    dropped[*index] = false;
                    changed = true;
                }
                if !dropped[*index] {
                    later.extend(
                        fields(claim)
                            .into_iter()
                            .flat_map(|fields| fields.keys().cloned()),
                    );
                    later_claims += 1;
                }
            }
        }
        for members in operations.values().chain(envelope_claims.values()) {
            if members.iter().any(|index| dropped[*index])
                && members.iter().any(|index| !dropped[*index])
            {
                for index in members {
                    dropped[*index] = false;
                }
                changed = true;
            }
        }
        for (envelope, members) in &envelope_claims {
            if records.get(envelope).copied() != Some(members.len())
                && members.iter().any(|index| dropped[*index])
            {
                for index in members {
                    dropped[*index] = false;
                }
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    let mut by_kind: BTreeMap<String, DropCount> = BTreeMap::new();
    for index in first.iter().copied() {
        let count = by_kind.entry(claims[index].claim.kind.clone()).or_default();
        count.sealed += 1;
        if dropped[index] {
            count.dropped += 1;
        }
    }
    by_kind.retain(|_, count| count.dropped > 0);
    let dropped_envelopes = envelope_claims
        .iter()
        .filter(|(_, members)| members.iter().all(|index| dropped[*index]))
        .map(|(key, _)| *key)
        .collect::<BTreeSet<_>>();
    let envelopes = sealed
        .envelopes
        .iter()
        .filter(|envelope| dropped_envelopes.contains(&envelope.key))
        .map(|envelope| EnvelopeTombstone {
            writer: envelope.key.writer.clone(),
            sequence: envelope.key.sequence,
            envelope_hash: envelope.key.envelope_hash.clone(),
            accepted_at_unix_ms: envelope.accepted_at_unix_ms,
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let tombstones = first
        .iter()
        .copied()
        .filter(|index| dropped[*index])
        .map(|index| claim_tombstone(&claims[index]))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let retained = first
        .iter()
        .copied()
        .filter(|index| !dropped[*index])
        .map(|index| claims[index].claim.id.as_str())
        .collect::<BTreeSet<_>>();
    DropPlan {
        cut_unix_ms: cut,
        rules_digest: rules_digest(),
        sealed_envelopes: sealed.envelopes.len(),
        sealed_claims: first.len(),
        sealed_digest: sealed_digest(sealed),
        drop_digest: drop_digest(&envelopes, &tombstones),
        retained_digest: retained_digest(retained.iter().copied()),
        envelopes,
        claims: tombstones,
        by_kind,
    }
}

fn claim_tombstone(sealed_claim: &SealedClaim) -> ClaimTombstone {
    let claim = &sealed_claim.claim;
    let operation = operation_parts(&claim.body);
    ClaimTombstone {
        id: claim.id.clone(),
        writer: sealed_claim.envelope.writer.clone(),
        sequence: sealed_claim.envelope.sequence,
        envelope_hash: sealed_claim.envelope.envelope_hash.clone(),
        subject: claim.subject.clone(),
        kind: claim.kind.clone(),
        actor: claim.actor.clone(),
        predecessors: claim.predecessors.clone(),
        operation_id: operation.map(|(id, _)| id.to_owned()),
        request_digest: operation.map(|(_, digest)| digest.to_owned()),
        accepted_at_unix_ms: claim.accepted_at_unix_ms,
    }
}

/// SHA-256 over the sealed set's envelope identities, in the inventory's encoding.
pub fn sealed_digest(sealed: &SealedSet) -> String {
    let identities = sealed
        .envelopes
        .iter()
        .map(|envelope| &envelope.key)
        .collect::<BTreeSet<_>>();
    let mut digest = Sha256::new();
    digest.update(b"st3-checkpoint-sealed-v1\0");
    for key in identities {
        update_identity_digest(&mut digest, &key.writer, key.sequence, &key.envelope_hash);
    }
    hex::encode(digest.finalize())
}

fn digest_field(digest: &mut Sha256, value: Option<&str>) {
    match value {
        Some(value) => {
            digest.update([1_u8]);
            digest.update((value.len() as u64).to_be_bytes());
            digest.update(value.as_bytes());
        }
        None => digest.update([0_u8]),
    }
}

/// SHA-256 over every field of every tombstone, sorted. A manifest whose tombstones differ in
/// any field from what the participants verified has a different digest.
pub fn drop_digest(envelopes: &[EnvelopeTombstone], claims: &[ClaimTombstone]) -> String {
    let mut digest = Sha256::new();
    digest.update(b"st3-checkpoint-drop-v1\0");
    let envelopes = envelopes.iter().collect::<BTreeSet<_>>();
    let claims = claims.iter().collect::<BTreeSet<_>>();
    digest.update((envelopes.len() as u64).to_be_bytes());
    for envelope in envelopes {
        digest_field(&mut digest, Some(&envelope.writer));
        digest_field(&mut digest, Some(&envelope.sequence.to_string()));
        digest_field(&mut digest, Some(&envelope.envelope_hash));
        digest_field(&mut digest, Some(&envelope.accepted_at_unix_ms.to_string()));
    }
    digest.update((claims.len() as u64).to_be_bytes());
    for claim in claims {
        digest_field(&mut digest, Some(&claim.id));
        digest_field(&mut digest, Some(&claim.writer));
        digest_field(&mut digest, Some(&claim.sequence.to_string()));
        digest_field(&mut digest, Some(&claim.envelope_hash));
        digest_field(&mut digest, Some(&claim.subject));
        digest_field(&mut digest, Some(&claim.kind));
        digest_field(&mut digest, claim.actor.as_deref());
        digest.update((claim.predecessors.len() as u64).to_be_bytes());
        for predecessor in &claim.predecessors {
            digest_field(&mut digest, Some(predecessor));
        }
        digest_field(&mut digest, claim.operation_id.as_deref());
        digest_field(&mut digest, claim.request_digest.as_deref());
        digest_field(&mut digest, Some(&claim.accepted_at_unix_ms.to_string()));
    }
    hex::encode(digest.finalize())
}

/// SHA-256 over the sorted IDs of the claims a checkpoint keeps before its cut.
pub fn retained_digest<'a>(ids: impl IntoIterator<Item = &'a str>) -> String {
    let mut digest = Sha256::new();
    digest.update(b"st3-checkpoint-retained-v1\0");
    for id in ids.into_iter().collect::<BTreeSet<_>>() {
        digest_field(&mut digest, Some(id));
    }
    hex::encode(digest.finalize())
}

/// The outcome of proving a drop plan on a copy of the store.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointProof {
    /// The graph projected from every claim before the cut.
    pub graph_digest_before: String,
    /// The graph projected from the kept claims before the cut.
    pub graph_digest: String,
    pub reader_digest_before: String,
    pub reader_digest: String,
    /// Subjects whose readers were compared.
    pub subjects: usize,
    pub passed: bool,
    /// The first readers that answered differently, as `subject reader`.
    pub mismatches: Vec<String>,
}

/// Tables projected from claims, children before the tables their foreign keys name.
const PROJECTION_TABLES: [&str; 17] = [
    "operations",
    "desired",
    "documents",
    "events",
    "mission_revisions",
    "mission_definitions",
    "mission_run_deadlines",
    "mission_run_after",
    "step_runs",
    "revision_proposals",
    "run_generations",
    "mission_runs",
    "planning_previews",
    "planning_candidates",
    "planning_sessions",
    "projection_health",
    "local_work_lease_renewals",
];

/// Clear every projection and replay the claims into it from nothing, as a new node would.
fn replay_from_nothing(transaction: &Transaction<'_>) -> Result<()> {
    for table in PROJECTION_TABLES {
        let exists: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
            [table],
            |row| row.get(0),
        )?;
        if exists {
            transaction.execute(&format!("DELETE FROM {table}"), [])?;
        }
    }
    rebuild_operations_tx(transaction)?;
    project_replicated_base_claims(transaction)?;
    project_replicated_mission_runs(transaction)?;
    rebuild_planning_tx(transaction)?;
    Ok(())
}

/// Record the tombstones of a checkpoint's drop. Recording them again changes nothing.
pub(crate) fn record_checkpoint_tombstones_tx(
    transaction: &Transaction<'_>,
    checkpoint: &str,
    envelopes: &[EnvelopeTombstone],
    claims: &[ClaimTombstone],
) -> Result<()> {
    let mut insert_envelope = transaction.prepare_cached(
        "INSERT OR IGNORE INTO checkpoint_envelopes(
             writer, sequence, envelope_hash, accepted_at_unix_ms, checkpoint)
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )?;
    for envelope in envelopes {
        insert_envelope.execute(params![
            envelope.writer,
            envelope.sequence,
            envelope.envelope_hash,
            i64::try_from(envelope.accepted_at_unix_ms)?,
            checkpoint
        ])?;
    }
    let mut insert_claim = transaction.prepare_cached(
        "INSERT OR IGNORE INTO checkpoint_claims(
             id, writer, sequence, envelope_hash, subject, kind, actor, predecessors,
             operation_id, request_digest, accepted_at_unix_ms, checkpoint)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
    )?;
    for claim in claims {
        insert_claim.execute(params![
            claim.id,
            claim.writer,
            claim.sequence,
            claim.envelope_hash,
            claim.subject,
            claim.kind,
            claim.actor,
            serde_json::to_string(&claim.predecessors)?,
            claim.operation_id,
            claim.request_digest,
            i64::try_from(claim.accepted_at_unix_ms)?,
            checkpoint
        ])?;
    }
    Ok(())
}

/// Delete dropped claims and their envelopes from a store. The trim uses the same code.
pub(crate) fn delete_dropped_rows_tx(
    transaction: &Transaction<'_>,
    envelopes: &[EnvelopeTombstone],
    claims: &[ClaimTombstone],
) -> Result<()> {
    for claim in claims {
        if let Some(operation) = &claim.operation_id {
            transaction.execute(
                "DELETE FROM operations WHERE id=?1 AND canonical_claim_id=?2",
                params![operation, claim.id],
            )?;
        }
        transaction.execute(
            "DELETE FROM events WHERE store_index IN (SELECT store_index FROM claims WHERE id=?1)",
            [&claim.id],
        )?;
        transaction.execute("DELETE FROM claims WHERE id=?1", [&claim.id])?;
    }
    for envelope in envelopes {
        let batch: Option<String> = transaction
            .query_row(
                "SELECT batch_id FROM replica_envelopes
                 WHERE writer=?1 AND sequence=?2 AND envelope_hash=?3",
                params![envelope.writer, envelope.sequence, envelope.envelope_hash],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        for table in [
            "replica_records",
            "replica_envelope_signatures",
            "replica_envelope_holds",
            "replica_envelopes",
        ] {
            transaction.execute(
                &format!(
                    "DELETE FROM {table} WHERE writer=?1 AND sequence=?2 AND envelope_hash=?3"
                ),
                params![envelope.writer, envelope.sequence, envelope.envelope_hash],
            )?;
        }
        if let Some(batch) = batch {
            transaction.execute(
                "DELETE FROM batches WHERE id=?1
                 AND NOT EXISTS (SELECT 1 FROM claims WHERE batch_id=?1)
                 AND NOT EXISTS (SELECT 1 FROM replica_envelopes WHERE batch_id=?1)",
                [&batch],
            )?;
        }
    }
    Ok(())
}

fn claims_of_kind_in_order(
    connection: &Connection,
    subject: &str,
    kind: &str,
) -> Result<Vec<ClaimRecord>> {
    connection
        .prepare_cached(&format!(
            "SELECT {CLAIM_COLUMNS} FROM claims JOIN batches ON batches.id=claims.batch_id
             WHERE claims.subject=?1 AND claims.kind=?2 ORDER BY {CANONICAL_ORDER}"
        ))?
        .query_map(params![subject, kind], claim_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

/// Every answer about `subject` that a checkpoint must leave unchanged, as of the cut.
fn subject_answers(connection: &Connection, subject: &str, cut: u128) -> Result<Value> {
    let mut answers = serde_json::Map::new();
    answers.insert(
        "actual".into(),
        json!(latest_actual_at(connection, subject, None)?),
    );
    answers.insert(
        "harness".into(),
        json!(current_harness_at(connection, subject, None)?),
    );
    // Which claim a status shows, its origin, and whether its runtime observations conflict.
    answers.insert(
        "source".into(),
        json!(selected_actual_source_at(connection, subject, None, None)?),
    );
    let kinds = connection
        .prepare_cached("SELECT DISTINCT kind FROM claims WHERE subject=?1 ORDER BY kind")?
        .query_map([subject], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut latest = serde_json::Map::new();
    for kind in &kinds {
        let claims = claims_of_kind_in_order(connection, subject, kind)?;
        if let Some(claim) = claims.last() {
            latest.insert(kind.clone(), json!(claim.id));
        }
        match kind.as_str() {
            "harness.observed" => {
                let incarnations = claims
                    .iter()
                    .filter_map(|claim| field_str(claim, "incarnation_id"))
                    .collect::<BTreeSet<_>>();
                let mut harness = serde_json::Map::new();
                for incarnation in incarnations {
                    let claims = claims
                        .iter()
                        .filter(|claim| field_str(claim, "incarnation_id") == Some(incarnation))
                        .collect::<Vec<_>>();
                    let state = |claim: &ClaimRecord| field_str(claim, "state").map(str::to_owned);
                    let ready = claims.iter().any(|claim| {
                        matches!(state(claim).as_deref(), Some("ready" | "working" | "idle"))
                    });
                    let ready_without_login = claims.iter().any(|claim| {
                        matches!(state(claim).as_deref(), Some("ready" | "working" | "idle"))
                            && field_str(claim, "reason") != Some("providerAuth")
                    });
                    let after = claims
                        .iter()
                        .rposition(|claim| state(claim).is_some_and(|state| state != "working"))
                        .map_or(0, |position| position + 1);
                    let working_since = claims[after..]
                        .iter()
                        .find(|claim| state(claim).as_deref() == Some("working"))
                        .map(|claim| claim.accepted_at_unix_ms.to_string());
                    harness.insert(
                        incarnation.to_owned(),
                        json!({
                            "ready": ready,
                            "ready_without_login": ready_without_login,
                            "working_since": working_since,
                        }),
                    );
                }
                answers.insert("incarnations".into(), Value::Object(harness));
            }
            "loop.state" => {
                answers.insert(
                    "loop".into(),
                    json!({
                        "latest": claims.last().map(|claim| &claim.body),
                        "items": claims
                            .iter()
                            .find(|claim| fields(claim).is_some_and(|fields| fields.contains_key("items")))
                            .map(|claim| &claim.body),
                    }),
                );
            }
            "subscription.mission-deferred" => {
                let pending = connection
                    .prepare_cached(
                        "SELECT request.id FROM claims AS request
                         WHERE request.subject=?1 AND request.kind='subscription.mission-requested'
                           AND NOT EXISTS (
                             SELECT 1 FROM claims AS finished
                             WHERE finished.subject=request.subject
                               AND finished.kind IN (
                                 'subscription.mission-started',
                                 'subscription.mission-failed',
                                 'subscription.mission-request-cancelled'
                               )
                               AND json_extract(finished.body, '$.fields.request')=request.id
                           )
                         ORDER BY request.id",
                    )?
                    .query_map([subject], |row| row.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                let mut deferrals = serde_json::Map::new();
                for request in pending {
                    let own = claims
                        .iter()
                        .filter(|claim| field_str(claim, "request") == Some(request.as_str()))
                        .collect::<Vec<_>>();
                    deferrals.insert(
                        request,
                        json!({
                            "count": own.len(),
                            "not_before": own.last().map(|claim| field_text(claim, "not_before_unix_ms")),
                        }),
                    );
                }
                answers.insert("deferrals".into(), Value::Object(deferrals));
            }
            _ => {}
        }
    }
    answers.insert("latest".into(), Value::Object(latest));
    let timing_claims = connection
        .prepare_cached(&format!(
            "SELECT {CLAIM_COLUMNS} FROM claims JOIN batches ON batches.id=claims.batch_id
             WHERE claims.subject=?1 AND claims.kind IN (
                 'step-run.state','step-run.carried','work.claimed','work.renewed',
                 'work.progress','work.submitted','work.failed','work.released')
             ORDER BY {CANONICAL_ORDER}"
        ))?
        .query_map([subject], claim_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let attempts = timing_claims
        .iter()
        .filter(|claim| claim.kind.starts_with("work."))
        .filter_map(|claim| {
            fields(claim)
                .and_then(|fields| fields.get("attempt"))
                .and_then(Value::as_u64)
        })
        .collect::<BTreeSet<_>>();
    if !attempts.is_empty() {
        let events = timing_claims.iter().map(timing_event).collect::<Vec<_>>();
        let mut timing = serde_json::Map::new();
        for attempt in attempts {
            let Ok(attempt) = u32::try_from(attempt) else {
                continue;
            };
            timing.insert(
                attempt.to_string(),
                json!(
                    timing_answers(&events, attempt, cut)
                        .into_iter()
                        .map(|(started, elapsed)| json!([
                            started.map(|value| value.to_string()),
                            elapsed.to_string()
                        ]))
                        .collect::<Vec<_>>()
                ),
            );
        }
        answers.insert("timing".into(), Value::Object(timing));
    }
    Ok(Value::Object(answers))
}

fn reader_answers(
    connection: &Connection,
    subjects: &BTreeSet<String>,
    cut: u128,
) -> Result<BTreeMap<String, Value>> {
    subjects
        .iter()
        .map(|subject| Ok((subject.clone(), subject_answers(connection, subject, cut)?)))
        .collect()
}

fn answers_digest(answers: &BTreeMap<String, Value>) -> Result<String> {
    let mut digest = Sha256::new();
    digest.update(b"st3-checkpoint-readers-v1\0");
    for (subject, value) in answers {
        digest_field(&mut digest, Some(subject));
        // Canonical text, so equal answers digest alike whatever order their keys were built in.
        digest_field(&mut digest, Some(&canonical_json_text(value)?));
    }
    Ok(hex::encode(digest.finalize()))
}

fn answer_mismatches(
    before: &BTreeMap<String, Value>,
    after: &BTreeMap<String, Value>,
) -> Vec<String> {
    let mut mismatches = Vec::new();
    for (subject, before) in before {
        let after = after.get(subject).cloned().unwrap_or(Value::Null);
        let (Some(before), Some(after)) = (before.as_object(), after.as_object()) else {
            mismatches.push(format!("{subject} all"));
            continue;
        };
        for (reader, value) in before {
            if after.get(reader) != Some(value) {
                mismatches.push(format!("{subject} {reader}"));
            }
        }
        for reader in after.keys() {
            if !before.contains_key(reader) {
                mismatches.push(format!("{subject} {reader}"));
            }
        }
    }
    mismatches.truncate(20);
    mismatches
}

/// Project a copy of the sealed set with and without the drop, and compare the graph and every
/// reader answer. `copy` is a store file holding at least the sealed set; it is changed.
pub fn prove_on_copy(copy: &Path, sealed: &SealedSet, plan: &DropPlan) -> Result<CheckpointProof> {
    let mut connection = Connection::open(copy)?;
    connection.execute_batch("PRAGMA foreign_keys = ON; PRAGMA journal_mode = MEMORY;")?;
    let transaction = connection.transaction()?;
    // Keep only the sealed set.
    transaction.execute_batch(
        "CREATE TEMP TABLE sealed_claims(id TEXT PRIMARY KEY);
         CREATE TEMP TABLE sealed_envelopes(writer TEXT, sequence INTEGER, envelope_hash TEXT,
             PRIMARY KEY(writer, sequence, envelope_hash));",
    )?;
    for sealed_claim in &sealed.claims {
        transaction.execute(
            "INSERT OR IGNORE INTO temp.sealed_claims(id) VALUES (?1)",
            [&sealed_claim.claim.id],
        )?;
    }
    for envelope in &sealed.envelopes {
        transaction.execute(
            "INSERT OR IGNORE INTO temp.sealed_envelopes VALUES (?1, ?2, ?3)",
            params![
                envelope.key.writer,
                envelope.key.sequence,
                envelope.key.envelope_hash
            ],
        )?;
    }
    for table in PROJECTION_TABLES {
        let exists: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
            [table],
            |row| row.get(0),
        )?;
        if exists {
            transaction.execute(&format!("DELETE FROM {table}"), [])?;
        }
    }
    transaction.execute_batch(
        "DELETE FROM local_observations;
         DELETE FROM claims WHERE id NOT IN (SELECT id FROM temp.sealed_claims);
         DELETE FROM replica_records WHERE NOT EXISTS (
             SELECT 1 FROM temp.sealed_envelopes s WHERE s.writer=replica_records.writer
               AND s.sequence=replica_records.sequence AND s.envelope_hash=replica_records.envelope_hash);",
    )?;
    // Number the claims in canonical order. Every node that holds the sealed set then proves
    // on the same store, whatever order its claims arrived in, so readers that compare store
    // indexes answer alike and the digests can match across nodes.
    transaction.execute_batch(&format!(
        "CREATE TEMP TABLE canonical_index(old INTEGER PRIMARY KEY, new INTEGER NOT NULL);
         INSERT INTO temp.canonical_index(old, new)
             SELECT claims.store_index, ROW_NUMBER() OVER (ORDER BY {CANONICAL_ORDER})
             FROM claims JOIN batches ON batches.id=claims.batch_id;
         UPDATE claims SET store_index=-(SELECT new FROM temp.canonical_index
                                         WHERE old=claims.store_index);
         UPDATE claims SET store_index=-store_index;"
    ))?;
    let subjects = plan
        .claims
        .iter()
        .map(|claim| claim.subject.clone())
        .collect::<BTreeSet<_>>();
    replay_from_nothing(&transaction)?;
    let graph_digest_before = graph_digest(&transaction)?;
    let before = reader_answers(&transaction, &subjects, sealed.cut_unix_ms)?;
    // As a trim does: tombstones first, which readers that walk ancestry pass through.
    record_checkpoint_tombstones_tx(
        &transaction,
        &checkpoint_name(sealed.cut_unix_ms),
        &plan.envelopes,
        &plan.claims,
    )?;
    delete_dropped_rows_tx(&transaction, &plan.envelopes, &plan.claims)?;
    replay_from_nothing(&transaction)?;
    let graph_digest_after = graph_digest(&transaction)?;
    let after = reader_answers(&transaction, &subjects, sealed.cut_unix_ms)?;
    let mut mismatches = answer_mismatches(&before, &after);
    if graph_digest_before != graph_digest_after {
        mismatches.insert(0, "graph".into());
    }
    let proof = CheckpointProof {
        reader_digest_before: answers_digest(&before)?,
        reader_digest: answers_digest(&after)?,
        graph_digest_before,
        graph_digest: graph_digest_after,
        subjects: subjects.len(),
        passed: mismatches.is_empty(),
        mismatches,
    };
    transaction.rollback()?;
    Ok(proof)
}

impl Store {
    /// The envelopes this node holds from before `cut_unix_ms`, and their admitted claims in
    /// canonical order. An envelope with any claim dated at or after the cut is not before it.
    pub fn checkpoint_sealed_set(&self, cut_unix_ms: u128) -> Result<SealedSet> {
        {
            // Every local batch has an envelope before the planner reads them.
            let mut connection = self.connection.write();
            let transaction = connection.transaction()?;
            seed_replica_envelopes_tx(&transaction, &self.origin, None)?;
            transaction.commit()?;
        }
        let connection = self.readers.get();
        let mut envelopes = connection
            .prepare(
                "SELECT envelopes.writer, envelopes.sequence, envelopes.envelope_hash,
                        envelopes.accepted_at_unix_ms,
                        (SELECT COUNT(*) FROM replica_records records
                         WHERE records.writer=envelopes.writer AND records.sequence=envelopes.sequence
                           AND records.envelope_hash=envelopes.envelope_hash)
                 FROM replica_envelopes envelopes",
            )?
            .query_map([], |row| {
                Ok(SealedEnvelope {
                    key: EnvelopeKey {
                        writer: row.get(0)?,
                        sequence: row.get(1)?,
                        envelope_hash: row.get(2)?,
                    },
                    accepted_at_unix_ms: row
                        .get::<_, String>(3)?
                        .parse()
                        .unwrap_or(u128::MAX),
                    records: row.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        envelopes.retain(|envelope| envelope.accepted_at_unix_ms < cut_unix_ms);
        let protected = connection
            .prepare(
                "SELECT claim_id FROM desired WHERE claim_id IS NOT NULL
                 UNION SELECT claim_id FROM mission_definitions
                 UNION SELECT claim_id FROM mission_revisions
                 UNION SELECT binding_claim_id FROM documents WHERE binding_claim_id IS NOT NULL
                 UNION SELECT json_extract(body, '$.fields.replacement') FROM claims
                     WHERE kind='record.repaired'
                 UNION SELECT replacement_claim_id FROM replica_records
                     WHERE replacement_claim_id IS NOT NULL",
            )?
            .query_map([], |row| row.get::<_, Option<String>>(0))?
            .filter_map(|row| row.transpose())
            .collect::<rusqlite::Result<BTreeSet<_>>>()?;
        let claims = connection
            .prepare(&format!(
                "SELECT {CLAIM_COLUMNS}, records.writer, records.sequence, records.envelope_hash,
                        records.state
                 FROM claims JOIN batches ON batches.id=claims.batch_id
                 JOIN replica_records records ON records.claim_id=claims.id
                 ORDER BY {CANONICAL_ORDER}"
            ))?
            .query_map([], |row| {
                Ok((
                    claim_from_row(row)?,
                    EnvelopeKey {
                        writer: row.get(10)?,
                        sequence: row.get(11)?,
                        envelope_hash: row.get(12)?,
                    },
                    row.get::<_, String>(13)? == "valid",
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut late = BTreeSet::new();
        for (claim, envelope, _) in &claims {
            if claim.accepted_at_unix_ms >= cut_unix_ms {
                late.insert(envelope.clone());
            }
        }
        envelopes.retain(|envelope| !late.contains(&envelope.key));
        let before = envelopes
            .iter()
            .map(|envelope| envelope.key.clone())
            .collect::<BTreeSet<_>>();
        let claims = claims
            .into_iter()
            .filter(|(_, envelope, _)| before.contains(envelope))
            .map(|(claim, envelope, valid)| SealedClaim {
                protected: protected.contains(&claim.id),
                claim,
                envelope,
                valid,
            })
            .collect();
        envelopes.sort_by(|left, right| left.key.cmp(&right.key));
        Ok(SealedSet {
            cut_unix_ms,
            envelopes,
            claims,
        })
    }

    /// Copy this store to `copy` from one consistent snapshot, while the writer carries on.
    fn copy_store_to(&self, copy: &Path) -> Result<()> {
        // Readers are read-only, and `VACUUM INTO` needs a connection that may write the copy.
        let flags = if self.shared_memory {
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_URI
        } else {
            OpenFlags::SQLITE_OPEN_READ_WRITE
        };
        let connection = Connection::open_with_flags(&self.path, flags)?;
        connection.execute_batch("PRAGMA busy_timeout = 5000;")?;
        connection.execute("VACUUM INTO ?1", [copy.to_string_lossy()])?;
        Ok(())
    }

    /// Plan the drop for `cut_unix_ms` and prove it on a copy of this store in `scratch`, which
    /// must be a directory the proof may write to. Nothing in this store changes.
    pub fn plan_checkpoint(
        &self,
        cut_unix_ms: u128,
        scratch: &Path,
    ) -> Result<(DropPlan, CheckpointProof)> {
        let sealed = self.checkpoint_sealed_set(cut_unix_ms)?;
        let plan = plan_drops(&sealed);
        fs::create_dir_all(scratch)?;
        let copy = scratch.join(format!("proof-{}.sqlite3", Uuid::now_v7().simple()));
        let result = self
            .copy_store_to(&copy)
            .and_then(|()| prove_on_copy(&copy, &sealed, &plan));
        for suffix in ["", "-journal", "-wal", "-shm"] {
            let _ = fs::remove_file(format!("{}{suffix}", copy.display()));
        }
        Ok((plan, result?))
    }

    /// `st replication checkpoint plan`: plan and prove one checkpoint without changing anything.
    pub fn checkpoint_plan_view(
        &self,
        cut_unix_ms: u128,
        scratch: &Path,
    ) -> Result<CheckpointPlanView> {
        let (plan, proof) = self.plan_checkpoint(cut_unix_ms, scratch)?;
        Ok(CheckpointPlanView {
            checkpoint: checkpoint_name(cut_unix_ms),
            cut_unix_ms,
            rules_digest: plan.rules_digest,
            sealed_envelopes: plan.sealed_envelopes,
            sealed_claims: plan.sealed_claims,
            sealed_digest: plan.sealed_digest,
            dropped_envelopes: plan.envelopes.len(),
            dropped_claims: plan.claims.len(),
            by_kind: plan.by_kind,
            drop_digest: plan.drop_digest,
            retained_digest: plan.retained_digest,
            proof,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const CUT: u128 = 20 * DAY_MS;

    /// Builds a sealed set by hand: each claim in its own envelope unless grouped, in canonical
    /// order. Every writer also gets a newest envelope that no rule drops, so the newest-envelope
    /// guard stays out of the way unless a test wants it.
    #[derive(Default)]
    struct Sealed {
        claims: Vec<(u128, String, u64, usize, SealedClaim)>,
        envelopes: Vec<SealedEnvelope>,
        sequences: BTreeMap<String, u64>,
        next: u64,
    }

    struct Draft<'a> {
        kind: &'a str,
        subject: &'a str,
        actor: Option<&'a str>,
        body: Value,
    }

    fn draft<'a>(kind: &'a str, subject: &'a str, fields: Value) -> Draft<'a> {
        Draft {
            kind,
            subject,
            actor: None,
            body: json!({ "fields": fields }),
        }
    }

    impl Sealed {
        fn add(&mut self, origin: &str, at: u128, draft: Draft<'_>) -> String {
            self.envelope(origin, at, vec![draft]).remove(0)
        }

        fn envelope(&mut self, origin: &str, at: u128, drafts: Vec<Draft<'_>>) -> Vec<String> {
            let sequence = self.sequences.entry(origin.to_owned()).or_default();
            *sequence += 1;
            let key = EnvelopeKey {
                writer: origin.into(),
                sequence: *sequence,
                envelope_hash: format!("hash-{origin}-{sequence}"),
            };
            self.envelopes.push(SealedEnvelope {
                key: key.clone(),
                accepted_at_unix_ms: at,
                records: drafts.len(),
            });
            let mut ids = Vec::new();
            for (position, draft) in drafts.into_iter().enumerate() {
                self.next += 1;
                let id = format!("claim-{}", self.next);
                let operation = operation_parts(&draft.body)
                    .map(|(id, digest)| (id.to_owned(), digest.to_owned()));
                self.claims.push((
                    at,
                    origin.to_owned(),
                    key.sequence,
                    position,
                    SealedClaim {
                        claim: ClaimRecord {
                            id: id.clone(),
                            store_index: self.next,
                            batch_id: format!("batch/{origin}/{}", key.sequence),
                            subject: draft.subject.into(),
                            kind: draft.kind.into(),
                            origin: origin.into(),
                            actor: draft.actor.map(str::to_owned),
                            operation_id: operation.as_ref().map(|(id, _)| id.clone()),
                            request_digest: operation.map(|(_, digest)| digest),
                            body: draft.body,
                            predecessors: Vec::new(),
                            accepted_at_unix_ms: at,
                        },
                        envelope: key.clone(),
                        valid: true,
                        protected: false,
                    },
                ));
                ids.push(id);
            }
            ids
        }

        fn claim_mut(&mut self, id: &str) -> &mut SealedClaim {
            self.claims
                .iter_mut()
                .map(|(.., claim)| claim)
                .find(|claim| claim.claim.id == id)
                .unwrap()
        }

        fn build(mut self) -> SealedSet {
            for writer in self.sequences.keys().cloned().collect::<Vec<_>>() {
                self.add(
                    &writer,
                    CUT - 1,
                    draft("daemon.started", "daemon/filler", json!({})),
                );
            }
            self.claims.sort_by(|left, right| {
                (left.0, &left.1, left.2, left.3).cmp(&(right.0, &right.1, right.2, right.3))
            });
            self.envelopes
                .sort_by(|left, right| left.key.cmp(&right.key));
            SealedSet {
                cut_unix_ms: CUT,
                envelopes: self.envelopes,
                claims: self.claims.into_iter().map(|(.., claim)| claim).collect(),
            }
        }
    }

    fn dropped(plan: &DropPlan) -> BTreeSet<String> {
        plan.claims.iter().map(|claim| claim.id.clone()).collect()
    }

    fn ids<const N: usize>(ids: [&String; N]) -> BTreeSet<String> {
        ids.into_iter().cloned().collect()
    }

    const T: u128 = CUT - 10 * DAY_MS;

    #[test]
    fn latest_rules_keep_the_newest_claim_of_each_slot() {
        let mut sealed = Sealed::default();
        let observer = [1, 2, 3].map(|offset| {
            sealed.add(
                "alder",
                T + offset,
                draft(
                    "observer.observed",
                    "observer/pulls",
                    json!({"status": "ok", "count": offset}),
                ),
            )
        });
        let code_a = [10, 11].map(|offset| {
            sealed.add(
                "alder",
                T + offset,
                draft(
                    "daemon.diagnostic",
                    "daemon/alder",
                    json!({"code": "slow-request", "message": "slow"}),
                ),
            )
        });
        let code_b = sealed.add(
            "alder",
            T + 12,
            draft(
                "daemon.diagnostic",
                "daemon/alder",
                json!({"code": "other", "message": "slow"}),
            ),
        );
        let link = |sealed: &mut Sealed, origin: &str, at| {
            sealed.add(
                origin,
                at,
                draft("transport.observed", "host/cedar", json!({"status": "up"})),
            )
        };
        let from_alder = [
            link(&mut sealed, "alder", T + 20),
            link(&mut sealed, "alder", T + 22),
        ];
        let from_birch = link(&mut sealed, "birch", T + 21);
        let plan = plan_drops(&sealed.build());
        assert_eq!(
            dropped(&plan),
            ids([&observer[0], &observer[1], &code_a[0], &from_alder[0]])
        );
        let _ = (code_b, from_birch);
        assert_eq!(
            plan.by_kind["observer.observed"],
            DropCount {
                sealed: 3,
                dropped: 2
            }
        );
    }

    #[test]
    fn local_kinds_go_only_once_five_days_older_than_the_cut() {
        let mut sealed = Sealed::default();
        let entry = |sealed: &mut Sealed, at| {
            sealed.add(
                "alder",
                at,
                draft(
                    "harness.timeline",
                    "agent/alder.worker",
                    json!({"incarnation_id": "inc-1", "entry": at.to_string()}),
                ),
            )
        };
        let old = [
            entry(&mut sealed, CUT - 6 * DAY_MS),
            entry(&mut sealed, CUT - 6 * DAY_MS + 1),
        ];
        let young = [
            entry(&mut sealed, CUT - 4 * DAY_MS),
            entry(&mut sealed, CUT - 3 * DAY_MS),
        ];
        let action = |sealed: &mut Sealed, actor: Option<&'static str>, at| {
            let mut draft = draft(
                "runtime.action.succeeded",
                "agent/alder.worker",
                json!({"action": "stop", "incarnation_id": "inc-1", "operation_status": "succeeded"}),
            );
            draft.actor = actor;
            sealed.add("alder", at, draft)
        };
        let system = [
            action(&mut sealed, None, CUT - 9 * DAY_MS),
            action(&mut sealed, None, CUT - 8 * DAY_MS),
        ];
        let person = [
            action(&mut sealed, Some("person/avery"), CUT - 9 * DAY_MS + 5),
            action(&mut sealed, Some("person/avery"), CUT - 8 * DAY_MS + 5),
        ];
        let plan = plan_drops(&sealed.build());
        assert_eq!(dropped(&plan), ids([&old[0], &old[1], &system[0]]));
        let _ = (young, person);
    }

    fn harness(state: &str, reason: Option<&str>) -> Value {
        json!({
            "state": state,
            "incarnation_id": "inc-1",
            "driver": "claude",
            "transport": "claude-channel",
            "reason": reason,
            "blocked_on": null,
            "ask": null,
            "input_buffer": null,
            "exit": null,
        })
    }

    #[test]
    fn harness_rule_keeps_every_position_a_reader_reads() {
        let mut sealed = Sealed::default();
        let agent = "agent/alder.worker";
        let states = [
            ("starting", None),
            ("ready", Some("providerAuth")),
            ("ready", None),
            ("working", None),
            ("idle", None),
            ("working", None),
            ("working", None),
            ("working", None),
        ];
        let claims = states
            .iter()
            .enumerate()
            .map(|(offset, (state, reason))| {
                sealed.add(
                    "alder",
                    T + offset as u128,
                    draft("harness.observed", agent, harness(state, *reason)),
                )
            })
            .collect::<Vec<_>>();
        // A legacy observation without an incarnation stays whatever follows it.
        let legacy = sealed.add(
            "alder",
            T - 1,
            draft("harness.observed", agent, json!({"state": "idle"})),
        );
        let plan = plan_drops(&sealed.build());
        // Kept: the first, the first ready, the first ready without a login prompt, the last
        // idle, every working after it, and the newest.
        assert_eq!(dropped(&plan), ids([&claims[3]]));
        assert!(!dropped(&plan).contains(&legacy));
    }

    /// `agent_working_since` over one incarnation's states in canonical order: the first
    /// `working` after the last other state.
    fn working_since(states: &[(Option<String>, u128)]) -> Option<u128> {
        let after = states
            .iter()
            .rposition(|(state, _)| state.as_deref().is_some_and(|state| state != "working"))
            .map_or(0, |position| position + 1);
        states[after..]
            .iter()
            .find(|(state, _)| state.as_deref() == Some("working"))
            .map(|(_, at)| *at)
    }

    fn harness_state() -> impl Strategy<Value = Option<&'static str>> {
        prop_oneof![
            4 => Just(Some("working")),
            2 => Just(Some("idle")),
            1 => Just(Some("ready")),
            1 => Just(Some("blocked")),
            1 => Just(None),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(512))]

        /// A late observation from a writer outside the sealed set can land anywhere in an
        /// incarnation's history. The answers read from the kept observations stay the same as
        /// those read from all of them.
        #[test]
        fn late_harness_observations_never_change_what_readers_answer(
            states in proptest::collection::vec(harness_state(), 1..30),
            late in harness_state(),
            late_at in 0u128..40,
        ) {
            let mut sealed = Sealed::default();
            for (offset, state) in states.iter().enumerate() {
                let mut body = json!({"incarnation_id": "inc-1"});
                if let Some(state) = state {
                    body["state"] = json!(state);
                }
                sealed.add("alder", T + offset as u128, draft("harness.observed", AGENT, body));
            }
            let set = sealed.build();
            let gone = dropped(&plan_drops(&set));
            let observed = |claim: &ClaimRecord| {
                (field_str(claim, "state").map(str::to_owned), claim.accepted_at_unix_ms)
            };
            let harness = set
                .claims
                .iter()
                .filter(|claim| claim.claim.kind == "harness.observed")
                .collect::<Vec<_>>();
            let full = harness.iter().map(|claim| observed(&claim.claim)).collect::<Vec<_>>();
            let kept = harness
                .iter()
                .filter(|claim| !gone.contains(&claim.claim.id))
                .map(|claim| observed(&claim.claim))
                .collect::<Vec<_>>();
            let ever_ready = |states: &[(Option<String>, u128)]| {
                states.iter().any(|(state, _)| {
                    matches!(state.as_deref(), Some("ready" | "working" | "idle"))
                })
            };
            prop_assert_eq!(working_since(&full), working_since(&kept));
            prop_assert_eq!(ever_ready(&full), ever_ready(&kept));
            let late = (late.map(str::to_owned), T + late_at);
            let with_late = |states: &[(Option<String>, u128)]| {
                let mut states = states.to_vec();
                let position = states.partition_point(|state| state.1 <= late.1);
                states.insert(position, late.clone());
                states
            };
            prop_assert_eq!(working_since(&with_late(&full)), working_since(&with_late(&kept)));
            prop_assert_eq!(ever_ready(&with_late(&full)), ever_ready(&with_late(&kept)));
        }
    }

    #[test]
    fn loop_rule_keeps_the_ends_of_each_round_and_the_first_items() {
        let mut sealed = Sealed::default();
        let subject = "loop-run/alder";
        let states = [
            json!({"loop": "loop/a", "status": "running", "round": 1}),
            json!({"loop": "loop/a", "status": "running", "round": 1, "items": ["x"]}),
            json!({"loop": "loop/a", "status": "running", "round": 1}),
            json!({"loop": "loop/a", "status": "running", "round": 1}),
            json!({"loop": "loop/a", "status": "running", "round": 2}),
            json!({"loop": "loop/a", "status": "running", "round": 2}),
            json!({"loop": "loop/a", "status": "running", "round": 2}),
            json!({"loop": "loop/a", "status": "done", "round": 2}),
        ];
        let claims = states
            .into_iter()
            .enumerate()
            .map(|(offset, fields)| {
                sealed.add(
                    "alder",
                    T + offset as u128,
                    draft("loop.state", subject, fields),
                )
            })
            .collect::<Vec<_>>();
        let plan = plan_drops(&sealed.build());
        assert_eq!(dropped(&plan), ids([&claims[2], &claims[5]]));
    }

    #[test]
    fn deferrals_stay_while_their_request_is_open() {
        let mut sealed = Sealed::default();
        let subject = "subscription/pulls";
        let deferral = |sealed: &mut Sealed, request: &str, attempt: u64, at| {
            sealed.add(
                "alder",
                at,
                draft(
                    "subscription.mission-deferred",
                    subject,
                    json!({"request": request, "attempt": attempt, "not_before_unix_ms": at + 60_000}),
                ),
            )
        };
        let closed = [
            deferral(&mut sealed, "claim-r1", 1, T + 1),
            deferral(&mut sealed, "claim-r1", 2, T + 2),
            deferral(&mut sealed, "claim-r1", 3, T + 3),
        ];
        sealed.add(
            "alder",
            T + 4,
            draft(
                "subscription.mission-started",
                subject,
                json!({"request": "claim-r1", "mission_run": "mission-run/x"}),
            ),
        );
        let open = [
            deferral(&mut sealed, "claim-r2", 1, T + 5),
            deferral(&mut sealed, "claim-r2", 2, T + 6),
        ];
        let plan = plan_drops(&sealed.build());
        assert_eq!(dropped(&plan), ids([&closed[0], &closed[1]]));
        let _ = open;
    }

    fn work(kind: &str, attempt: u64, expires: Option<u128>) -> Value {
        let mut fields =
            json!({"attempt": attempt, "status": "working", "claimant": "agent/alder.worker"});
        if let Some(expires) = expires {
            fields["claim_expires_at_unix_ms"] = json!(expires as u64);
        }
        let _ = kind;
        fields
    }

    fn step_events(sealed: &mut Sealed, events: &[(&str, u128, Option<u128>)]) -> Vec<String> {
        events
            .iter()
            .map(|(kind, at, expires)| {
                let fields = if kind.starts_with("step-run.") {
                    json!({"status": if *expires == Some(0) { "completed" } else { "working" }})
                } else {
                    work(kind, 1, *expires)
                };
                sealed.add("alder", T + at, draft(kind, "step-run/s/build", fields))
            })
            .collect()
    }

    /// No renewal goes. The timing fold closes an interval when the next event arrives after the
    /// expiry that stands, so a late lease event from a writer outside the sealed set, landing
    /// before a renewal, would make that renewal decide the answer. This is the case proptest
    /// found for a rule that dropped renewals the next renewal made redundant.
    #[test]
    fn renewals_stay_because_a_late_lease_can_need_any_of_them() {
        let mut sealed = Sealed::default();
        step_events(
            &mut sealed,
            &[
                ("work.claimed", 0, Some(T + 30)),
                ("work.renewed", 4, Some(T + 30)),
                ("work.renewed", 18, Some(T + 19)),
            ],
        );
        let set = sealed.build();
        assert!(dropped(&plan_drops(&set)).is_empty());
        let events = set
            .claims
            .iter()
            .map(|claim| timing_event(&claim.claim))
            .collect::<Vec<_>>();
        let late = (
            "work.progress".to_owned(),
            json!({"fields": work("work.progress", 1, Some(T + 4))}),
            T,
        );
        let with_late = |events: &[TimingEvent]| {
            let mut events = events.to_vec();
            events.insert(1, late.clone());
            events
        };
        let without_first_renewal = [events[0].clone(), events[2].clone()];
        assert_eq!(
            fold_step_timing(&with_late(&events), 1, u128::MAX, false),
            (None, 19)
        );
        assert_eq!(
            fold_step_timing(&with_late(&without_first_renewal), 1, u128::MAX, false),
            (None, 4)
        );
    }

    #[test]
    fn the_design_reviews_renewal_schedule_keeps_every_renewal() {
        // Claimed at 0 until 10; renewed at 5 until 20 and at 15 until 30. Without the renewal
        // at 5, the lease would lapse at 10 and the interval would close before 15.
        let mut sealed = Sealed::default();
        let claims = step_events(
            &mut sealed,
            &[
                ("work.claimed", 0, Some(T + 10)),
                ("work.renewed", 5, Some(T + 20)),
                ("work.renewed", 15, Some(T + 30)),
            ],
        );
        let set = sealed.build();
        let plan = plan_drops(&set);
        assert!(dropped(&plan).is_empty(), "{:?}", dropped(&plan));
        let events = set
            .claims
            .iter()
            .map(|claim| timing_event(&claim.claim))
            .collect::<Vec<_>>();
        assert_eq!(fold_step_timing(&events, 1, T + 25, true), (Some(T), 25));
        let _ = claims;
    }

    #[test]
    fn a_renewal_that_shortens_the_lease_stays() {
        let mut sealed = Sealed::default();
        let claims = step_events(
            &mut sealed,
            &[
                ("work.claimed", 0, Some(T + 100)),
                ("work.renewed", 10, Some(T + 50)),
                ("work.renewed", 20, Some(T + 200)),
            ],
        );
        assert!(dropped(&plan_drops(&sealed.build())).is_empty());
        let _ = claims;
    }

    #[test]
    fn guards_keep_claims_a_rule_would_drop() {
        let observed = |at: u128| {
            draft(
                "observer.observed",
                "observer/pulls",
                json!({"status": "ok", "at": at.to_string()}),
            )
        };
        let mut sealed = Sealed::default();
        let mut by_person = observed(1);
        by_person.actor = Some("person/avery");
        let person = sealed.add("alder", T + 1, by_person);
        let invalid = sealed.add("alder", T + 2, observed(2));
        let protected = sealed.add("alder", T + 3, observed(3));
        let cited = sealed.add("alder", T + 4, observed(4));
        let mut with_operation = observed(5);
        with_operation.body["_operation"] =
            json!({"id": "operation/shared", "request_digest": "d"});
        let shared = sealed.add("alder", T + 5, with_operation);
        let mut citing = draft(
            "attention.requested",
            "attention/look",
            json!({"reason": "look"}),
        );
        citing.body["evidence"] = json!([cited]);
        citing.body["_operation"] = json!({"id": "operation/shared", "request_digest": "d"});
        sealed.add("alder", T + 6, citing);
        let plain = sealed.add("alder", T + 7, observed(7));
        let newest = sealed.add("alder", T + 8, observed(8));
        sealed.claim_mut(&invalid).valid = false;
        sealed.claim_mut(&protected).protected = true;
        let plan = plan_drops(&sealed.build());
        assert_eq!(dropped(&plan), ids([&plain]));
        let _ = (person, shared, newest);
    }

    #[test]
    fn each_writers_newest_envelope_before_the_cut_stays() {
        let mut sealed = Sealed::default();
        let first = sealed.add(
            "alder",
            T,
            draft("observer.observed", "observer/a", json!({"status": "ok"})),
        );
        let second = sealed.add(
            "alder",
            T + 1,
            draft("observer.observed", "observer/a", json!({"status": "ok"})),
        );
        let mut set = sealed.build();
        // Take the filler away, so the second observation is in alder's newest envelope.
        set.claims
            .retain(|claim| claim.claim.kind != "daemon.started");
        set.envelopes.pop();
        let plan = plan_drops(&set);
        assert_eq!(dropped(&plan), ids([&first]));
        let _ = second;
    }

    #[test]
    fn an_envelope_goes_only_when_every_claim_in_it_goes() {
        let mut sealed = Sealed::default();
        let both = sealed.envelope(
            "alder",
            T,
            vec![
                draft("observer.observed", "observer/a", json!({"status": "ok"})),
                draft("observer.observed", "observer/b", json!({"status": "ok"})),
            ],
        );
        let half = sealed.envelope(
            "alder",
            T + 1,
            vec![
                draft("observer.observed", "observer/c", json!({"status": "ok"})),
                draft("mission.produced", "mission/x", json!({})),
            ],
        );
        for subject in ["observer/a", "observer/b", "observer/c"] {
            sealed.add(
                "alder",
                T + 2,
                draft("observer.observed", subject, json!({"status": "ok"})),
            );
        }
        let plan = plan_drops(&sealed.build());
        assert_eq!(dropped(&plan), ids([&both[0], &both[1]]));
        assert_eq!(plan.envelopes.len(), 1);
        assert_eq!(plan.envelopes[0].sequence, 1);
        let _ = half;
    }

    #[test]
    fn a_claim_whose_witness_lacks_one_of_its_fields_stays() {
        let mut sealed = Sealed::default();
        let wider = sealed.add(
            "alder",
            T,
            draft(
                "observer.observed",
                "observer/a",
                json!({"status": "ok", "extra": 1}),
            ),
        );
        let narrower = sealed.add(
            "alder",
            T + 1,
            draft("observer.observed", "observer/a", json!({"status": "ok"})),
        );
        let plan = plan_drops(&sealed.build());
        assert!(dropped(&plan).is_empty());
        let _ = (wider, narrower);
    }

    #[test]
    fn later_claims_can_witness_a_claim_field_by_field() {
        let mut sealed = Sealed::default();
        let observed = |fields| draft("observer.observed", "observer/a", fields);
        let both = sealed.add("alder", T, observed(json!({"status": "ok", "cursor": "1"})));
        let status = sealed.add("alder", T + 1, observed(json!({"status": "ok"})));
        let cursor = sealed.add("alder", T + 2, observed(json!({"cursor": "2"})));
        let plan = plan_drops(&sealed.build());
        // The newest carrier of each field stays; the first claim's fields are both set again.
        assert_eq!(dropped(&plan), ids([&both]));
        let _ = (status, cursor);
    }

    #[test]
    fn a_claim_held_in_two_envelopes_stays() {
        let mut sealed = Sealed::default();
        let older = sealed.add(
            "alder",
            T,
            draft("observer.observed", "observer/a", json!({"status": "ok"})),
        );
        let newer = sealed.add(
            "alder",
            T + 1,
            draft("observer.observed", "observer/a", json!({"status": "ok"})),
        );
        let mut set = sealed.build();
        // A second envelope of the same writer and sequence, under its legacy hash, holds the
        // newest claim again.
        let copy = set
            .claims
            .iter()
            .find(|claim| claim.claim.id == newer)
            .unwrap()
            .clone();
        let key = EnvelopeKey {
            envelope_hash: "legacy".into(),
            ..copy.envelope.clone()
        };
        set.envelopes.push(SealedEnvelope {
            key: key.clone(),
            accepted_at_unix_ms: T + 1,
            records: 1,
        });
        let position = set
            .claims
            .iter()
            .position(|claim| claim.claim.id == newer)
            .unwrap();
        set.claims.insert(
            position + 1,
            SealedClaim {
                envelope: key,
                ..copy
            },
        );
        set.envelopes
            .sort_by(|left, right| left.key.cmp(&right.key));
        let plan = plan_drops(&set);
        // Without the guard the first copy of the newest claim would be dropped, witnessed by
        // its own second copy.
        assert_eq!(dropped(&plan), ids([&older]));
        assert_eq!(
            plan.by_kind["observer.observed"],
            DropCount {
                sealed: 2,
                dropped: 1
            }
        );
    }

    #[test]
    fn the_drop_digest_covers_every_tombstone_field() {
        let envelope = EnvelopeTombstone {
            writer: "alder".into(),
            sequence: 4,
            envelope_hash: "hash".into(),
            accepted_at_unix_ms: 10,
        };
        let claim = ClaimTombstone {
            id: "claim-1".into(),
            writer: "alder".into(),
            sequence: 4,
            envelope_hash: "hash".into(),
            subject: "observer/a".into(),
            kind: "observer.observed".into(),
            actor: None,
            predecessors: vec!["claim-0".into(), "claim-00".into()],
            operation_id: Some("operation/a".into()),
            request_digest: Some("digest".into()),
            accepted_at_unix_ms: 10,
        };
        let base = drop_digest(
            std::slice::from_ref(&envelope),
            std::slice::from_ref(&claim),
        );
        let mut changed_claims: Vec<ClaimTombstone> = Vec::new();
        let mut push = |change: fn(&mut ClaimTombstone)| {
            let mut claim = claim.clone();
            change(&mut claim);
            changed_claims.push(claim);
        };
        push(|claim| claim.writer = "birch".into());
        push(|claim| claim.sequence = 5);
        push(|claim| claim.envelope_hash = "other".into());
        push(|claim| claim.subject = "observer/b".into());
        push(|claim| claim.kind = "daemon.diagnostic".into());
        push(|claim| claim.actor = Some("person/avery".into()));
        push(|claim| claim.predecessors.pop().map(drop).unwrap_or_default());
        push(|claim| claim.predecessors.push("claim-000".into()));
        push(|claim| claim.predecessors.reverse());
        push(|claim| claim.operation_id = None);
        push(|claim| claim.operation_id = Some("operation/b".into()));
        push(|claim| claim.request_digest = Some("other".into()));
        push(|claim| claim.request_digest = None);
        push(|claim| claim.accepted_at_unix_ms = 11);
        for changed in changed_claims {
            assert_ne!(
                drop_digest(
                    std::slice::from_ref(&envelope),
                    std::slice::from_ref(&changed)
                ),
                base,
                "{changed:?}"
            );
        }
        let mut changed = envelope.clone();
        changed.accepted_at_unix_ms = 11;
        assert_ne!(drop_digest(&[changed], std::slice::from_ref(&claim)), base);
        assert_ne!(drop_digest(&[], std::slice::from_ref(&claim)), base);
        assert_ne!(drop_digest(std::slice::from_ref(&envelope), &[]), base);
        // Order does not matter; content does.
        let other = ClaimTombstone {
            id: "claim-2".into(),
            ..claim.clone()
        };
        assert_eq!(
            drop_digest(std::slice::from_ref(&envelope), &[claim.clone(), other.clone()]),
            drop_digest(&[envelope], &[other, claim])
        );
    }

    #[test]
    fn checkpoint_names_are_utc_days() {
        let cut = checkpoint_cut("2026-09-27").unwrap();
        assert_eq!(checkpoint_name(cut), "checkpoint/2026-09-27");
        assert_eq!(checkpoint_cut("checkpoint/2026-09-27").unwrap(), cut);
        assert_eq!(newest_due_cut(cut + 2 * DAY_MS), cut);
        assert_eq!(newest_due_cut(cut + 3 * DAY_MS - 1), cut);
        assert_eq!(newest_due_cut(cut + 3 * DAY_MS), cut + DAY_MS);
        assert!(checkpoint_cut("yesterday").is_err());
    }

    #[derive(Clone, Debug)]
    enum Step {
        Renew(u128, u128),
        Progress(u128, u128),
        Active(u128),
        Submit(u128),
        Claim(u128, u128),
    }

    fn step_strategy() -> impl Strategy<Value = Step> {
        prop_oneof![
            6 => (1u128..40, 1u128..60).prop_map(|(gap, lease)| Step::Renew(gap, lease)),
            2 => (1u128..40, 1u128..60).prop_map(|(gap, lease)| Step::Progress(gap, lease)),
            1 => (1u128..40).prop_map(Step::Active),
            1 => (1u128..40).prop_map(Step::Submit),
            1 => (1u128..40, 1u128..60).prop_map(|(gap, lease)| Step::Claim(gap, lease)),
        ]
    }

    fn timing_history(steps: &[Step]) -> Vec<(String, Value, u128)> {
        let mut at = T;
        let mut events = vec![(
            "work.claimed".to_owned(),
            json!({"fields": work("", 1, Some(T + 30))}),
            T,
        )];
        for step in steps {
            let (kind, gap, lease) = match step {
                Step::Renew(gap, lease) => ("work.renewed", *gap, Some(*lease)),
                Step::Progress(gap, lease) => ("work.progress", *gap, Some(*lease)),
                Step::Active(gap) => ("step-run.state", *gap, None),
                Step::Submit(gap) => ("work.submitted", *gap, None),
                Step::Claim(gap, lease) => ("work.claimed", *gap, Some(*lease)),
            };
            at += gap;
            let fields = if kind == "step-run.state" {
                json!({"status": "working"})
            } else {
                work(kind, 1, lease.map(|lease| at + lease))
            };
            events.push((kind.to_owned(), json!({ "fields": fields }), at));
        }
        events
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(512))]

        /// Dropped renewals never change a step's timing, now or after any late event of the
        /// same attempt is inserted anywhere in the history.
        #[test]
        fn dropped_renewals_never_change_step_timing(
            steps in proptest::collection::vec(step_strategy(), 1..30),
            late in step_strategy(),
            late_at in 0u128..1_200,
        ) {
            let history = timing_history(&steps);
            let mut sealed = Sealed::default();
            for (kind, body, at) in &history {
                sealed.add("alder", *at, Draft { kind, subject: "step-run/s/build", actor: None, body: body.clone() });
            }
            let set = sealed.build();
            let plan = plan_drops(&set);
            let gone = dropped(&plan);
            let kept = set.claims.iter().filter(|claim| !gone.contains(&claim.claim.id)).map(|claim| timing_event(&claim.claim)).collect::<Vec<_>>();
            let full = set.claims.iter().map(|claim| timing_event(&claim.claim)).collect::<Vec<_>>();
            for snapshot in [T + 50, T + 400, CUT, u128::MAX] {
                for active in [true, false] {
                    prop_assert_eq!(fold_step_timing(&full, 1, snapshot, active), fold_step_timing(&kept, 1, snapshot, active));
                }
            }
            // A late event from a writer that did not take part.
            let late = timing_history(&[late]).pop().unwrap();
            let late = (late.0, late.1, T + late_at);
            let with_late = |events: &[(String, Value, u128)]| {
                let mut events = events.to_vec();
                let position = events.partition_point(|event| event.2 <= late.2);
                events.insert(position, late.clone());
                events
            };
            for snapshot in [T + 400, u128::MAX] {
                for active in [true, false] {
                    prop_assert_eq!(
                        fold_step_timing(&with_late(&full), 1, snapshot, active),
                        fold_step_timing(&with_late(&kept), 1, snapshot, active)
                    );
                }
            }
        }
    }

    fn input(
        subject: &str,
        kind: &str,
        actor: Option<&str>,
        fields: Value,
        key: &str,
    ) -> ClaimInput {
        ClaimInput {
            subject: subject.into(),
            kind: kind.into(),
            actor: actor.map(str::to_owned),
            fields: fields
                .as_object()
                .unwrap()
                .iter()
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect(),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some(key.into()),
        }
    }

    const AGENT: &str = "agent/alder.worker";

    /// Claims of several rules' kinds, with some that a checkpoint drops.
    fn write_history(store: &Store) {
        store
            .append_claim(&input(
                AGENT,
                "runtime.observed",
                None,
                json!({"status": "running", "incarnation_id": "inc-1"}),
                "runtime",
            ))
            .unwrap();
        for (n, state) in ["idle", "working", "idle", "working", "idle", "working"]
            .iter()
            .enumerate()
        {
            store
                .append_claim_outcome(&input(
                    AGENT,
                    "harness.observed",
                    Some(AGENT),
                    json!({"state": state, "incarnation_id": "inc-1", "observed_at_ms": n}),
                    &format!("harness-{n}"),
                ))
                .unwrap();
        }
        for n in 0..4 {
            store
                .append_claim(&input(
                    "daemon/alder",
                    "daemon.diagnostic",
                    None,
                    json!({"severity": "warning", "code": "slow-request", "reason": format!("slow {n}")}),
                    &format!("diagnostic-{n}"),
                ))
                .unwrap();
        }
    }

    fn receive(target: &Store, relay: &str, exchange: &ReplicationExchange) {
        target.bind_fleet("fleet/test").unwrap();
        target
            .receive_replication_exchange(relay, "fleet/test", exchange)
            .unwrap();
        target.validate_replication_backlog().unwrap();
        target.project_replication_backlog().unwrap();
    }

    fn only(
        exchange: &ReplicationExchange,
        envelopes: Vec<ReplicaEnvelope>,
    ) -> ReplicationExchange {
        ReplicationExchange {
            inventory: ReplicationInventory {
                digest: String::new(),
                envelopes: envelopes
                    .iter()
                    .map(|envelope| ReplicaEnvelopeId {
                        writer: envelope.writer.clone(),
                        sequence: envelope.sequence,
                        hash: envelope.hash.clone(),
                    })
                    .collect(),
                buckets: Vec::new(),
                accepts: None,
            },
            envelopes,
            ..exchange.clone()
        }
    }

    #[test]
    fn the_proof_passes_for_the_plan_and_fails_for_a_drop_a_reader_needs() {
        let store = Store::open_memory("alder").unwrap();
        write_history(&store);
        let scratch = tempfile::tempdir().unwrap();
        let cut = now_ms() + 1_000;
        let (plan, proof) = store.plan_checkpoint(cut, scratch.path()).unwrap();
        assert!(proof.passed, "{proof:?}");
        assert_eq!(proof.graph_digest_before, proof.graph_digest);
        assert_eq!(proof.reader_digest_before, proof.reader_digest);
        assert!(
            plan.claims
                .iter()
                .any(|claim| claim.kind == "harness.observed")
                && plan
                    .claims
                    .iter()
                    .any(|claim| claim.kind == "daemon.diagnostic"),
            "{plan:?}"
        );
        assert_eq!(proof.graph_digest, proof.graph_digest_before);
        assert_eq!(proof.reader_digest, proof.reader_digest_before);

        let before = store.claims_for(AGENT, None).unwrap().len();
        let sealed = store.checkpoint_sealed_set(cut).unwrap();
        let newest = sealed
            .claims
            .iter()
            .rev()
            .find(|claim| claim.claim.kind == "harness.observed")
            .unwrap();
        let mut wrong = plan.clone();
        wrong.claims.push(claim_tombstone(newest));
        let copy = scratch.path().join("wrong.sqlite3");
        store.copy_store_to(&copy).unwrap();
        let proof = prove_on_copy(&copy, &sealed, &wrong).unwrap();
        assert!(!proof.passed);
        assert!(
            proof
                .mismatches
                .iter()
                .any(|mismatch| mismatch.starts_with(&format!("{AGENT} "))),
            "{:?}",
            proof.mismatches
        );
        // The live store is never changed by a proof.
        assert_eq!(store.claims_for(AGENT, None).unwrap().len(), before);
    }

    #[test]
    fn nodes_that_hold_the_same_claims_plan_the_same_drop() {
        let source = Store::open_memory("alder").unwrap();
        write_history(&source);
        source.bind_fleet("fleet/test").unwrap();
        let exchange = source
            .export_replication_exchange("fleet/test", &ReplicationInventory::default())
            .unwrap();
        assert!(exchange.envelopes.len() > 5);
        let in_order = Store::open_memory("birch").unwrap();
        receive(&in_order, "alder", &exchange);
        let reversed = Store::open_memory("cedar").unwrap();
        for envelope in exchange.envelopes.iter().rev() {
            receive(&reversed, "alder", &only(&exchange, vec![envelope.clone()]));
        }
        let cut = now_ms() + 1_000;
        let plans = [&source, &in_order, &reversed]
            .map(|store| plan_drops(&store.checkpoint_sealed_set(cut).unwrap()));
        assert!(!plans[0].claims.is_empty());
        for plan in &plans[1..] {
            assert_eq!(plan.sealed_digest, plans[0].sealed_digest);
            assert_eq!(plan.drop_digest, plans[0].drop_digest);
            assert_eq!(plan.retained_digest, plans[0].retained_digest);
            assert_eq!(plan.claims, plans[0].claims);
        }
        // The proofs agree too, although the nodes numbered their claims differently.
        let scratch = tempfile::tempdir().unwrap();
        let proofs = [&source, &in_order, &reversed]
            .map(|store| store.plan_checkpoint(cut, scratch.path()).unwrap().1);
        for proof in &proofs {
            assert!(proof.passed, "{proof:?}");
            assert_eq!(proof.reader_digest_before, proof.reader_digest);
            assert_eq!(proof.graph_digest, proofs[0].graph_digest);
            assert_eq!(proof.reader_digest, proofs[0].reader_digest);
        }
    }

    /// Nearly every claim cites the claim before it on its subject. A walk from a runtime's
    /// newest observation back to another writer's older one must pass through a dropped claim
    /// by its tombstone, or the status would show a runtime conflict that is not there.
    #[test]
    fn ancestry_walks_through_a_dropped_claim() {
        let birch = Store::open_memory("birch").unwrap();
        let older = birch
            .append_claim(&input(
                AGENT,
                "runtime.observed",
                None,
                json!({"status": "running", "incarnation_id": "inc-1"}),
                "birch-runtime",
            ))
            .unwrap();
        birch.bind_fleet("fleet/test").unwrap();
        let alder = Store::open_memory("alder").unwrap();
        receive(
            &alder,
            "birch",
            &birch
                .export_replication_exchange("fleet/test", &ReplicationInventory::default())
                .unwrap(),
        );
        let middle = alder
            .append_claim(&input(
                AGENT,
                "harness.observed",
                Some(AGENT),
                json!({"state": "working", "incarnation_id": "inc-1"}),
                "alder-harness",
            ))
            .unwrap();
        let newest = alder
            .append_claim(&input(
                AGENT,
                "runtime.observed",
                None,
                json!({"status": "running", "incarnation_id": "inc-1"}),
                "alder-runtime",
            ))
            .unwrap();
        assert_eq!(middle.predecessors, std::slice::from_ref(&older.id));
        assert_eq!(newest.predecessors, std::slice::from_ref(&middle.id));
        let source = |store: &Store| {
            selected_actual_source_at(&store.readers.get(), AGENT, None, None).unwrap()
        };
        assert_eq!(
            source(&alder),
            (Some(newest.id.clone()), Some("alder".into()), false)
        );

        let tombstone = ClaimTombstone {
            id: middle.id.clone(),
            writer: "alder".into(),
            sequence: 0,
            envelope_hash: String::new(),
            subject: middle.subject.clone(),
            kind: middle.kind.clone(),
            actor: middle.actor.clone(),
            predecessors: middle.predecessors.clone(),
            operation_id: middle.operation_id.clone(),
            request_digest: middle.request_digest.clone(),
            accepted_at_unix_ms: middle.accepted_at_unix_ms,
        };
        {
            let mut connection = alder.connection.write();
            let transaction = connection.transaction().unwrap();
            record_checkpoint_tombstones_tx(
                &transaction,
                "checkpoint/test",
                &[],
                std::slice::from_ref(&tombstone),
            )
            .unwrap();
            // Recording twice changes nothing.
            record_checkpoint_tombstones_tx(
                &transaction,
                "checkpoint/test",
                &[],
                std::slice::from_ref(&tombstone),
            )
            .unwrap();
            delete_dropped_rows_tx(&transaction, &[], std::slice::from_ref(&tombstone)).unwrap();
            assert!(claim_descends_from(&transaction, &newest.id, &older.id).unwrap());
            transaction.commit().unwrap();
        }
        assert!(alder.claim_by_id(&middle.id).unwrap().is_none());
        assert_eq!(
            source(&alder),
            (Some(newest.id.clone()), Some("alder".into()), false)
        );
    }

    /// Plans a checkpoint over a copy of a real store and prints the dry run:
    /// `ST3_CHECKPOINT_STORE=/var/tmp/copy.sqlite3 ST3_CHECKPOINT_ORIGIN=hetz
    /// ST3_CHECKPOINT_DAY=2026-09-27 cargo test -p st3 --lib plan_a_copy_of_a_real_store --
    /// --ignored --nocapture`. The copy is changed; the store it came from is not read.
    #[test]
    #[ignore = "reads the store copy named by ST3_CHECKPOINT_STORE"]
    fn plan_a_copy_of_a_real_store() {
        let path = PathBuf::from(std::env::var("ST3_CHECKPOINT_STORE").unwrap());
        let origin = std::env::var("ST3_CHECKPOINT_ORIGIN").unwrap_or_else(|_| "hetz".into());
        let day = std::env::var("ST3_CHECKPOINT_DAY").unwrap();
        let started = std::time::Instant::now();
        let store = Store::open(&path, origin).unwrap();
        eprintln!("opened in {:?}", started.elapsed());
        let scratch = path.parent().unwrap().join("proof");
        let plan = if std::env::var("ST3_CHECKPOINT_SKIP_PROOF").is_ok() {
            None
        } else {
            Some(
                store
                    .checkpoint_plan_view(checkpoint_cut(&day).unwrap(), &scratch)
                    .unwrap(),
            )
        };
        eprintln!("planned and proved in {:?}", started.elapsed());
        println!("{}", serde_json::to_string_pretty(&plan).unwrap());
        if let Ok(subject) = std::env::var("ST3_CHECKPOINT_SUBJECT") {
            let sealed = store
                .checkpoint_sealed_set(checkpoint_cut(&day).unwrap())
                .unwrap();
            let plan = plan_drops(&sealed);
            let dropped = dropped(&plan);
            for claim in sealed
                .claims
                .iter()
                .filter(|claim| claim.claim.subject == subject)
            {
                println!(
                    "{} {} {} valid={} protected={} envelope={}/{} dropped={}",
                    claim.claim.store_index,
                    claim.claim.kind,
                    claim.claim.id,
                    claim.valid,
                    claim.protected,
                    claim.envelope.writer,
                    claim.envelope.sequence,
                    dropped.contains(&claim.claim.id)
                );
            }
        }
    }
}
