//! Placement handoff is a graph fence, independent of peer liveness or wall-clock time.
//! A former owner acknowledges the selected placement only after its local runtime is gone.
use crate::model::{ClaimInput, ClaimRecord, DesiredSubject, St3Error};
use crate::store::Store;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

pub const SOURCE_OFFLINE_KIND: &str = "agent.placement.source-offline";

#[derive(Deserialize)]
pub struct SourceOfflineRequest {
    pub subject: String,
    pub actor: String,
    pub desired_token: String,
    pub sources: Vec<String>,
    pub idempotency_key: String,
}

/// The operator names an exact placement and sources; no peer timeout becomes stop evidence.
pub fn source_offline_input(
    store: &Store,
    request: SourceOfflineRequest,
) -> Result<ClaimInput, St3Error> {
    let invalid = |message| St3Error::new("invalid-source-offline", message);
    let claim = store
        .claim_by_id(&request.desired_token)
        .map_err(|e| St3Error::new("internal", e.to_string()))?
        .ok_or_else(|| invalid("placement declaration is missing"))?;
    if claim.subject != request.subject || claim.kind != "intent.desired" {
        return Err(invalid(
            "override must name this seat's placement declaration",
        ));
    }
    let fence = fence(store, &request.subject, &request.desired_token)
        .map_err(|e| St3Error::new("internal", e.to_string()))?
        .ok_or_else(|| invalid("override needs an agent placement"))?;
    let sources = request.sources.into_iter().collect::<BTreeSet<_>>();
    if sources.is_empty()
        || sources
            .iter()
            .any(|source| !fence.sources.contains_key(source))
    {
        return Err(invalid(
            "override must name former source hosts of this placement",
        ));
    }
    Ok(ClaimInput {
        subject: request.subject,
        kind: SOURCE_OFFLINE_KIND.into(),
        actor: Some(request.actor),
        fields: BTreeMap::from([
            (
                "desired_token".into(),
                Value::String(request.desired_token.clone()),
            ),
            (
                "destination".into(),
                Value::String(fence.destination.clone()),
            ),
            ("sources".into(), serde_json::json!(sources)),
        ]),
        evidence: vec![request.desired_token],
        expected_subject: None,
        idempotency_key: Some(request.idempotency_key),
    })
}

/// Only explicitly recorded overrides of this immutable placement release its sources.
pub fn source_offline_overrides(
    store: &Store,
    subject: &str,
    fence: &Fence,
    at: u64,
) -> Result<BTreeMap<String, ClaimRecord>> {
    let mut overrides = BTreeMap::new();
    for claim in store.claims_for(subject, Some(SOURCE_OFFLINE_KIND))? {
        if claim.store_index > at
            || field(&claim, "desired_token") != Some(fence.token.as_str())
            || field(&claim, "destination") != Some(fence.destination.as_str())
            || claim
                .actor
                .as_deref()
                .is_none_or(|actor| !actor.starts_with("person/") && !actor.starts_with("agent/"))
        {
            continue;
        }
        for source in claim.body["fields"]["sources"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .filter(|source| fence.sources.contains_key(*source))
        {
            overrides.insert(source.to_owned(), claim.clone());
        }
    }
    Ok(overrides)
}

pub fn field<'a>(claim: &'a ClaimRecord, key: &str) -> Option<&'a str> {
    claim
        .body
        .pointer(&format!("/fields/{key}"))
        .and_then(Value::as_str)
}

/// The acknowledgement cites an immutable declaration through ordinary claim evidence.
pub fn acknowledges(store: &Store, claim: &ClaimRecord, tokens: &BTreeSet<String>) -> Result<bool> {
    let mut pending = vec![claim.clone()];
    let mut seen = BTreeSet::new();
    while let Some(claim) = pending.pop() {
        if !seen.insert(claim.id.clone()) {
            continue;
        }
        if tokens.contains(&claim.id)
            || claim
                .body
                .get("evidence")
                .and_then(Value::as_array)
                .is_some_and(|evidence| {
                    evidence
                        .iter()
                        .filter_map(Value::as_str)
                        .any(|token| tokens.contains(token))
                })
        {
            return Ok(true);
        }
        for id in &claim.predecessors {
            if let Some(previous) = store.claim_by_id(id)?
                && previous.subject == claim.subject
            {
                pending.push(previous);
            }
        }
    }
    Ok(false)
}

/// The former owners and declarations which can acknowledge their latest departure.
/// Walk causal predecessors, never receiver-local indexes or receipt timestamps.
pub struct Fence {
    pub token: String,
    pub destination: String,
    pub sources: BTreeMap<String, BTreeSet<String>>,
}
pub fn fence(store: &Store, subject: &str, token: &str) -> Result<Option<Arc<Fence>>> {
    smallclaims::touched::note_read(|| subject.to_owned());
    store.cached_placement_fence(token, || build_fence(store, subject, token))
}
fn build_fence(store: &Store, subject: &str, token: &str) -> Result<Option<Fence>> {
    let Some(current) = store.claim_by_id(token)? else {
        return Ok(None);
    };
    let desired: DesiredSubject = serde_json::from_value(current.body.clone())?;
    let Some(member) = desired.member else {
        return Ok(None);
    };
    let destination = member.host.clone();
    let mut result = Fence {
        token: token.into(),
        destination: destination.clone(),
        sources: BTreeMap::new(),
    };
    let mut pending = vec![(current, BTreeSet::new(), BTreeSet::new())];
    while let Some((claim, newer, mut found_hosts)) = pending.pop() {
        let declaration: DesiredSubject = serde_json::from_value(claim.body.clone())?;
        if let Some(member) = declaration.member {
            let host = member.host.clone();
            if host != destination && found_hosts.insert(host.clone()) {
                // Acknowledgements at a declaration newer than this source's most recent
                // placement prove it learned the departure (including a stop-then-start).
                result
                    .sources
                    .entry(host)
                    .and_modify(|tokens| {
                        *tokens = tokens.intersection(&newer).cloned().collect();
                    })
                    .or_insert_with(|| newer.clone());
            }
        }
        let mut newer = newer;
        newer.insert(claim.id.clone());
        for predecessor in claim.predecessors {
            if newer.contains(&predecessor) {
                continue;
            }
            if let Some(previous) = store.claim_by_id(&predecessor)?
                && previous.subject == subject
                && previous.kind == "intent.desired"
            {
                pending.push((previous, newer.clone(), found_hosts.clone()));
            }
        }
    }
    Ok(Some(result))
}

#[derive(Serialize)]
pub struct Handoff {
    pub phase: &'static str,
    pub destination: String,
    pub sources: Vec<String>,
    pub pending_sources: Vec<String>,
    pub overridden_sources: Vec<String>,
    pub desired_token: String,
}

/// A source's newest observation is authoritative only for its own process.
pub fn latest_by_origin(
    store: &Store,
    subject: &str,
    at: u64,
) -> Result<BTreeMap<String, ClaimRecord>> {
    store.runtime_observations_at(subject, at)
}

pub fn handoff(store: &Store, subject: &str, token: &str, at: u64) -> Result<Option<Handoff>> {
    let Some(fence) = fence(store, subject, token)? else {
        return Ok(None);
    };
    if fence.sources.is_empty() {
        return Ok(None);
    }
    let latest = latest_by_origin(store, subject, at)?;
    let overrides = source_offline_overrides(store, subject, &fence, at)?;
    let mut pending_sources = Vec::new();
    for (host, tokens) in &fence.sources {
        let acknowledged = match latest.get(host) {
            Some(claim) if field(claim, "status") == Some("stopped") => {
                acknowledges(store, claim, tokens)?
            }
            _ => false,
        };
        if !acknowledged && !overrides.contains_key(host) {
            pending_sources.push(host.clone());
        }
    }
    let destination = latest.get(&fence.destination);
    let phase = if !pending_sources.is_empty() {
        "stopping-source"
    } else if destination.is_some_and(|c| field(c, "status") == Some("running")) {
        "running"
    } else if destination.is_some_and(|c| field(c, "status") == Some("starting")) {
        "starting"
    } else {
        "waiting-for-destination"
    };
    Ok(Some(Handoff {
        phase,
        destination: fence.destination.clone(),
        sources: fence.sources.keys().cloned().collect(),
        pending_sources,
        overridden_sources: overrides.keys().cloned().collect(),
        desired_token: fence.token.clone(),
    }))
}
