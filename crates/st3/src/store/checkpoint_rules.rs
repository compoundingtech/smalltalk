//! smalltalk's checkpoint rules: which of its claims a stable checkpoint may drop, what a
//! checkpoint proof compares about each subject, and the attention a stalled checkpoint asks
//! for. The graph runs checkpoints (`smallclaims::store::checkpoint`) and asks the runtime for
//! these through `smallclaims::store::Runtime`.
//!
//! `doc/fleet/smalltalk/checkpoint-design` is the design. Each rule keeps, within a slot, the
//! claims a reader's answer depends on. It drops a claim only when a later kept claim of the same
//! slot replaces it for every fold that reads the kind (the claim's witness). The planner is a
//! pure function of the claims before the cut, in canonical order, so every node that holds the
//! same claims drops the same ones.

use super::*;
use smallclaims::store::checkpoint::DAY_MS;
use smallclaims::store::checkpoint::*;
use smallclaims::store::checkpoint_agreement::*;

/// The rule engine's version. It is part of the rules digest, so nodes agree on a checkpoint only
/// when they run the same rules. Version 5 leaves repaired originals out of the sealed set and
/// proves on the sealed claims' blobs only. Version 6 applies work extensions to the canonical
/// graph and execution timing, so older builds seal different rules rather than publishing an
/// incompatible one-time graph or reader verification under the same terms. Version 7 selects
/// one initial definition when members independently create the same scheduled occurrence.
pub const RULES_VERSION: u32 = 7;

/// Kinds that are now local observations are dropped only when they are dated at least five days
/// before the cut, so they are seven days old when the checkpoint is due. That matches the local
/// observation log's default retention.
pub(crate) const LOCAL_KIND_MIN_AGE_MS: u128 = 5 * DAY_MS;

/// Usage rollups keep their hourly history for this long before the cut, so a usage period that
/// ends within it is exact to the hour. Before it, each rollup series keeps only its newest
/// snapshot: the series' total, and the baseline for a period that starts at the window's edge.
/// Every response is also exported to OpenTelemetry for history beyond the window.
pub(crate) const USAGE_WINDOW_MS: u128 = 7 * DAY_MS;
const HOUR_MS: u128 = 60 * 60 * 1000;

/// The optional fields `current_harness_at` folds newest first from a harness's observations.
pub(crate) const HARNESS_OPTIONAL_FIELDS: [&str; 7] = [
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
pub(crate) const REQUEST_CLOSERS: [&str; 3] = [
    "subscription.mission-started",
    "subscription.mission-failed",
    "subscription.mission-request-cancelled",
];

/// A canonical description of every rule. The rules digest hashes it with `RULES_VERSION`.
pub(crate) const RULES_DESCRIPTION: &str = "\
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
harness.usage semantics=response_rollup slot=subject,incarnation_id,model,account,owner_run,owner_step,host keep=newest,last-of-each-utc-hour-by-observed_at-within-7d-before-cut,newest-before-that
harness.usage semantics=session_cumulative slot=subject,incarnation_id keep=newest,largest-total_tokens
harness.usage semantics=context_occupancy slot=subject,incarnation_id keep=newest
harness.limits slot=subject keep=newest
resource.observed actor=null observer=set slot=subject keep=newest
render.applied slot=subject keep=newest min-age-before-cut=5d
runtime.readiness-deadline-reached slot=subject keep=newest min-age-before-cut=5d
sealed=every-admitted-claim-of-an-envelope-before-the-cut-but-repaired-originals
proof=the-sealed-claims-and-the-blobs-they-reference
guards=person-actor,once-cardinality,record-not-valid,repair-replacement,projection-reference,claim-in-two-envelopes,cited-as-evidence,mission-run-input,shared-operation,writer-newest-envelope,whole-envelope
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum Rule {
    Newest,
    NewestAged,
    HarnessObserved,
    LoopState,
    Deferral,
    UsageSeries,
    UsageCumulative,
}

pub(crate) fn fields(claim: &ClaimRecord) -> Option<&serde_json::Map<String, Value>> {
    claim.body.get("fields").and_then(Value::as_object)
}

pub(crate) fn field_text(claim: &ClaimRecord, name: &str) -> String {
    match fields(claim).and_then(|fields| fields.get(name)) {
        Some(Value::String(text)) => text.clone(),
        Some(value) => value.to_string(),
        None => String::new(),
    }
}

pub(crate) fn field_str<'a>(claim: &'a ClaimRecord, name: &str) -> Option<&'a str> {
    fields(claim)
        .and_then(|fields| fields.get(name))
        .and_then(Value::as_str)
}

/// The rule and slot of a claim, or `None` when no rule may drop it.
pub(crate) fn slot_of(claim: &ClaimRecord) -> Option<(Rule, Vec<String>)> {
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
        // Limits are read as each seat's newest reading.
        "harness.limits" => Some((Rule::Newest, slot(&[]))),
        "harness.todo.observed" => Some((Rule::Newest, slot(&[]))),
        // An observer records a resource's complete facts in every observation, so its newest
        // observation replaces the older ones. A repository observer records each item as its
        // own resource, so each item keeps its latest state. A version that a subscription request
        // or a message was made for shares their envelope and stays with them.
        "resource.observed"
            if claim.actor.is_none()
                && fields(claim).is_some_and(|fields| fields.contains_key("observer")) =>
        {
            Some((Rule::Newest, slot(&[])))
        }
        // A legacy per-response claim is summed by every usage read, so it stays.
        "harness.usage" => match field_str(claim, "semantics")? {
            "response_rollup" => Some((
                Rule::UsageSeries,
                slot(&[
                    "incarnation_id",
                    "model",
                    "account",
                    "owner_run",
                    "owner_step",
                    "host",
                ]),
            )),
            "session_cumulative" => Some((
                Rule::UsageCumulative,
                slot(&["semantics", "incarnation_id"]),
            )),
            "context_occupancy" => Some((Rule::Newest, slot(&["semantics", "incarnation_id"]))),
            _ => None,
        },
        _ => None,
    }
}

/// Whether every field `claim` sets is set again by a later kept claim of its slot, whose field
/// names are `later`. Folds read these kinds last writer wins, field by field, so the later
/// claims replace it whatever arrives in between. A state transition clears every field of its
/// kind, so any later claim of the kind replaces one that sets only schema fields.
pub(crate) fn witnessed(
    claim: &ClaimRecord,
    later: &BTreeSet<String>,
    later_claims: usize,
) -> bool {
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
pub(crate) fn field_carriers(claims: &[&ClaimRecord]) -> BTreeSet<usize> {
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

pub(crate) fn harness_keep(claims: &[&ClaimRecord]) -> BTreeSet<usize> {
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

/// When a usage snapshot was measured, as its writer recorded it, so every member buckets it the
/// same way.
fn usage_observed_at(claim: &ClaimRecord) -> u128 {
    fields(claim)
        .and_then(|fields| fields.get("observed_at_unix_ms"))
        .and_then(Value::as_u64)
        .map_or(claim.accepted_at_unix_ms, u128::from)
}

/// A rollup series is cumulative, so the snapshots kept are the ones a period read uses as its
/// ends: the last of each UTC hour within [`USAGE_WINDOW_MS`] before the cut, the newest before
/// that window as its baseline, and the newest of all as the series' total.
pub(crate) fn usage_series_keep(claims: &[&ClaimRecord], cut: u128) -> BTreeSet<usize> {
    let window = cut.saturating_sub(USAGE_WINDOW_MS);
    let mut last_of_hour = BTreeMap::<u128, usize>::new();
    let mut newest_before_window = None;
    for (position, claim) in claims.iter().enumerate() {
        let at = usage_observed_at(claim);
        if at < window {
            newest_before_window = Some(position);
        } else {
            last_of_hour.insert(at / HOUR_MS, position);
        }
    }
    let mut keep = last_of_hour.into_values().collect::<BTreeSet<_>>();
    keep.extend(newest_before_window);
    keep.extend(claims.len().checked_sub(1));
    keep
}

/// A session's cumulative usage is read as its largest reading, which is normally its newest.
pub(crate) fn usage_cumulative_keep(claims: &[&ClaimRecord]) -> BTreeSet<usize> {
    let total = |claim: &ClaimRecord| {
        fields(claim)
            .and_then(|fields| fields.get("total_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    let mut keep = BTreeSet::from_iter(claims.len().checked_sub(1));
    // The fold replaces its reading with any later one at least as large, so it answers with
    // the last claim of the largest total.
    let largest = claims.iter().map(|claim| total(claim)).max().unwrap_or(0);
    keep.extend(
        claims
            .iter()
            .enumerate()
            .filter(|(_, claim)| total(claim) == largest)
            .map(|(position, _)| position)
            .last(),
    );
    keep
}

pub(crate) fn loop_keep(claims: &[&ClaimRecord]) -> BTreeSet<usize> {
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

pub(crate) type TimingEvent = (String, Value, u128);

pub(crate) fn timing_event(claim: &ClaimRecord) -> TimingEvent {
    (
        claim.kind.clone(),
        claim.body.clone(),
        claim.accepted_at_unix_ms,
    )
}

pub(crate) fn timing_answers(
    events: &[TimingEvent],
    attempt: u32,
    cut: u128,
) -> Vec<(Option<u128>, u128)> {
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
            Rule::UsageSeries => usage_series_keep(&slot_claims, cut),
            Rule::UsageCumulative => usage_cumulative_keep(&slot_claims),
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
    // A claim cited as evidence, or pinned as a mission run's input, is read by its ID.
    let cited = claims
        .iter()
        .flat_map(|sealed_claim| {
            let body = &sealed_claim.claim.body;
            let evidence = body
                .get("evidence")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str);
            let inputs = (sealed_claim.claim.kind == "mission-run.created")
                .then(|| body.pointer("/fields/inputs").and_then(Value::as_object))
                .flatten()
                .into_iter()
                .flat_map(|inputs| inputs.values())
                .filter_map(|input| input.get("claim_id").and_then(Value::as_str));
            evidence.chain(inputs)
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
        // Cumulative: every tombstone before the cut, from earlier checkpoints and this one. A
        // node that adopts the checkpoint checks the whole manifest against it.
        drop_digest: drop_digest(
            &[sealed.envelope_tombstones.as_slice(), envelopes.as_slice()].concat(),
            &[sealed.claim_tombstones.as_slice(), tombstones.as_slice()].concat(),
        ),
        retained_digest: retained_digest(retained.iter().copied()),
        envelopes,
        claims: tombstones,
        by_kind,
    }
}

/// Tables projected from claims, children before the tables their foreign keys name.
pub(crate) const PROJECTION_TABLES: [&str; 20] = [
    "operations",
    "arrangement_registers",
    "arrangements",
    "resource_observations",
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
pub(crate) fn replay_from_nothing(transaction: &Transaction<'_>) -> Result<()> {
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
    resources::rebuild(transaction)?;
    arrangements::rebuild(transaction)?;
    Ok(())
}

/// Every answer about `subject` that a checkpoint must leave unchanged, as of the cut.
pub(crate) fn subject_answers(connection: &Connection, subject: &str, cut: u128) -> Result<Value> {
    let mut answers = serde_json::Map::new();
    if subject.starts_with("glass/") {
        let person = st3_schema::glasses::owner(subject).map_err(anyhow::Error::new)?;
        answers.insert(
            "glasses".into(),
            json!(super::glasses::glasses_at(
                connection,
                person,
                i64::MAX as u64
            )?),
        );
    }
    if subject.starts_with("arrangement/") {
        let person = st3_schema::arrangements::owner(subject).map_err(anyhow::Error::new)?;
        answers.insert("arrangements".into(), json!(super::arrangements::arrangements_at(connection, person, i64::MAX as u64)?));
        answers.insert("arrangement".into(), json!(super::arrangements::arrangement_at(connection, subject, i64::MAX as u64)?));
    }
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
            // A rollup trim keeps every series' total, so lifetime usage must not change.
            "harness.usage" => {
                let rows = claims
                    .iter()
                    .map(|claim| {
                        Ok((
                            claim.store_index,
                            serde_json::to_string(&claim.body).unwrap_or_default(),
                            claim.accepted_at_unix_ms.to_string(),
                        ))
                    })
                    .collect::<Vec<rusqlite::Result<_>>>();
                answers.insert(
                    "usage".into(),
                    json!(Store::usage_summary_from_rows(rows, None)?),
                );
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
                 'work.progress','work.submitted','work.failed','work.released','work.extended')
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

impl Store {
    /// The first checkpoint after the newest stable one that is still not stable
    /// `CHECKPOINT_ATTENTION_AFTER_MS` after it became due. The participant that sorts first
    /// among those that sealed asks the person to bring the others back or excuse them.
    pub(crate) fn checkpoint_attention_items(
        &self,
        person: Option<&str>,
        as_of: u128,
    ) -> Result<Vec<AttentionItemView>> {
        if person.is_some_and(|person| person != "person/operator") {
            return Ok(Vec::new());
        }
        let claims = self.checkpoint_claims()?;
        let newest_stable = stable_checkpoints(&claims).keys().next_back().copied();
        let due = newest_due_cut(as_of);
        let cut = claims
            .iter()
            .filter_map(CheckpointClaim::seal_terms)
            .map(|terms| terms.cut_unix_ms)
            .filter(|cut| *cut <= due && newest_stable.is_none_or(|stable| *cut > stable))
            .max();
        let Some(cut) = cut else {
            return Ok(Vec::new());
        };
        let checkpoint = checkpoint_name(cut);
        let seals = newest_seals(&claims, &checkpoint);
        let Some((writer, terms)) = seals.first_key_value() else {
            return Ok(Vec::new());
        };
        let first_waiting = newest_stable.map_or_else(
            || {
                claims
                    .iter()
                    .filter_map(CheckpointClaim::seal_terms)
                    .map(|terms| terms.cut_unix_ms)
                    .min()
                    .unwrap_or(cut)
            },
            |stable| stable + DAY_MS,
        );
        let since = first_waiting + 2 * DAY_MS;
        if as_of < since + CHECKPOINT_ATTENTION_AFTER_MS {
            return Ok(Vec::new());
        }
        let left = self.checkpoint_left_writers()?;
        let current = participants(&BTreeSet::new(), &left, &claims);
        let waiting = terms
            .participants
            .intersection(&current)
            .filter(|writer| seals.get(*writer) != Some(terms))
            .cloned()
            .collect::<Vec<_>>();
        if waiting.is_empty() {
            return Ok(Vec::new());
        }
        Ok(vec![AttentionItemView {
            episode: format!("{}:{writer}", checkpoint_name(first_waiting)), priority: "normal".into(), kind: "fault".into(),
            review_mode: None, subject: checkpoint.clone(), person: "person/operator".into(), requester_id: None,
            launch_id: None, variant_id: None, message_id: None, title: format!("Checkpoints are waiting for {}", waiting.join(", ")),
            detail: "Bring the waiting machines back, upgrade them, or excuse a machine that stays away.".into(),
            mission: None, mission_run: None, step: None, targets: vec![checkpoint.clone()], requested_at_unix_ms: since,
            actions: vec![attention_action("inspect checkpoint", &["st", "replication", "checkpoint", "status"])],
            request: None,
        }])
    }
}

/// Empty every projection and the local observation log on a checkpoint proof's copy, before
/// the graph cuts its claims down to the sealed set.
pub(crate) fn clear_projections(transaction: &Transaction<'_>) -> Result<()> {
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
    transaction.execute_batch("DELETE FROM local_observations;")?;
    Ok(())
}
