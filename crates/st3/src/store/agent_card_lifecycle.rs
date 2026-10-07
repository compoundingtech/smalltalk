//! Namespace-owned handoff and suspension dependencies for the full agent card.
//! Source adapters supply complete current rows and explicit negative lookups. Only
//! bounded dirty-agent maintenance walks indexed causal inputs; reads never repair.
//! This dependency does not attach a registry/Publisher or certify a full card source cut.
//! It has no local clock inputs. Claim, desired and active canonical operation captures
//! must come from the unified owner's same source transaction. Missing inputs stay dirty;
//! bounds/invalid inputs leave a sticky namespace fence, requiring a replacement rebuild.
use super::*;
use crate::suspension::Suspension;
use serde::{Deserialize, Serialize};
use smallclaims::ivm::install::Namespace;

pub(crate) const FINGERPRINT: &str = "agent-lifecycle.v1;namespace.v1;live-placement.v1;ordered-presentation-lineage.v1;active-operation-selection.v1;bounds256-v1";
const BOUND: usize = 256;
fn sql(ns: &Namespace, query: &str) -> String {
    query.replace("@NS@", &format!("'{}'", ns.as_str().replace('\'', "''")))
}
pub(crate) fn create_schema(c: &Connection) -> Result<()> {
    c.execute_batch(
        r#"
CREATE TABLE IF NOT EXISTS local_agent_lifecycle_claims(
 namespace TEXT NOT NULL, id TEXT NOT NULL, agent TEXT NOT NULL, kind TEXT NOT NULL,
 origin TEXT NOT NULL, rank BLOB NOT NULL, request INTEGER NOT NULL, record TEXT,
 PRIMARY KEY(namespace,id)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS agent_lifecycle_requests
 ON local_agent_lifecycle_claims(namespace,agent,request,rank DESC) WHERE request=1;
CREATE INDEX IF NOT EXISTS agent_lifecycle_runtime
 ON local_agent_lifecycle_claims(namespace,agent,kind,origin,rank DESC) WHERE record IS NOT NULL;
CREATE TABLE IF NOT EXISTS local_agent_lifecycle_overrides(
 namespace TEXT NOT NULL, agent TEXT NOT NULL, token TEXT NOT NULL, destination TEXT NOT NULL,
 source TEXT NOT NULL, id TEXT NOT NULL, PRIMARY KEY(namespace,agent,token,destination,source,id)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS agent_lifecycle_override_claim
 ON local_agent_lifecycle_overrides(namespace,id);
CREATE TABLE IF NOT EXISTS local_agent_lifecycle_desired(
 namespace TEXT NOT NULL, agent TEXT NOT NULL, token TEXT, kind TEXT,
 PRIMARY KEY(namespace,agent), CHECK((token IS NULL)=(kind IS NULL))
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS local_agent_lifecycle_operations(
 namespace TEXT NOT NULL, id TEXT NOT NULL, record TEXT,
 PRIMARY KEY(namespace,id)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS local_agent_lifecycle_dependencies(
 namespace TEXT NOT NULL, kind TEXT NOT NULL, key TEXT NOT NULL, agent TEXT NOT NULL,
 PRIMARY KEY(namespace,kind,key,agent)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS agent_lifecycle_dependency_owner
 ON local_agent_lifecycle_dependencies(namespace,agent,kind,key);
CREATE TABLE IF NOT EXISTS local_agent_lifecycle_rows(
 namespace TEXT NOT NULL, agent TEXT NOT NULL, body TEXT NOT NULL,
 PRIMARY KEY(namespace,agent)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS local_agent_lifecycle_dirty(
 namespace TEXT NOT NULL, agent TEXT NOT NULL, PRIMARY KEY(namespace,agent)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS local_agent_lifecycle_fences(
 namespace TEXT NOT NULL, agent TEXT NOT NULL, reason TEXT NOT NULL,
 PRIMARY KEY(namespace,agent)
) WITHOUT ROWID;
"#,
    )?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Need {
    Claim(String),
    Desired(String),
    Operation(String),
}
impl std::fmt::Display for Need {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "lifecycle input pending: {self:?}")
    }
}
impl std::error::Error for Need {}

fn queue(tx: &Transaction<'_>, ns: &Namespace, agent: &str) -> Result<()> {
    tx.execute(
        &sql(
            ns,
            "INSERT INTO local_agent_lifecycle_dirty VALUES(@NS@,?1) ON CONFLICT DO NOTHING",
        ),
        [agent],
    )?;
    Ok(())
}
fn fence(tx: &Transaction<'_>, ns: &Namespace, agent: &str, reason: &str) -> Result<()> {
    tx.execute(
        &sql(
            ns,
            "INSERT INTO local_agent_lifecycle_fences VALUES(@NS@,?1,?2)
      ON CONFLICT(namespace,agent) DO UPDATE SET reason=excluded.reason",
        ),
        params![agent, reason],
    )?;
    Ok(())
}
fn depend(tx: &Transaction<'_>, ns: &Namespace, agent: &str, kind: &str, key: &str) -> Result<()> {
    tx.execute(&sql(ns,"INSERT INTO local_agent_lifecycle_dependencies VALUES(@NS@,?1,?2,?3) ON CONFLICT DO NOTHING"),params![kind,key,agent])?;
    Ok(())
}
fn wake(tx: &Transaction<'_>, ns: &Namespace, kind: &str, key: &str) -> Result<BTreeSet<String>> {
    let agents = tx
        .prepare_cached(&sql(
            ns,
            "SELECT agent FROM local_agent_lifecycle_dependencies
      WHERE namespace=@NS@ AND kind=?1 AND key=?2 ORDER BY agent LIMIT 129",
        ))?
        .query_map(params![kind, key], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<BTreeSet<_>>>()?;
    anyhow::ensure!(
        agents.len() <= 128,
        "lifecycle reverse dependency fanout exceeds bound"
    );
    for agent in &agents {
        queue(tx, ns, agent)?;
    }
    Ok(agents)
}
fn sfield<'a>(c: &'a ClaimRecord, key: &str) -> Option<&'a str> {
    c.body
        .get("fields")
        .unwrap_or(&c.body)
        .get(key)
        .and_then(Value::as_str)
}
fn pfield<'a>(c: &'a ClaimRecord, key: &str) -> Option<&'a str> {
    crate::placement::field(c, key)
}
fn is_request(c: &ClaimRecord) -> bool {
    c.kind == "runtime.action.requested"
        && c.actor.is_some()
        && matches!(sfield(c, "action"), Some("suspend" | "resume"))
}

/// Every same-agent claim supplies causal links, including intermediate harness reports.
/// Needed foreign/absent IDs must also be supplied explicitly; no live source scan occurs.
pub(crate) fn apply_claim(
    tx: &Transaction<'_>,
    ns: &Namespace,
    old: Option<&ClaimRecord>,
    new: Option<(&ClaimRecord, &canonical::ClaimKey)>,
) -> Result<BTreeSet<String>> {
    let mut dirty = BTreeSet::new();
    if let Some(old) = old {
        dirty.extend(wake(tx, ns, "claim", &old.id)?);
        tx.execute(
            &sql(
                ns,
                "DELETE FROM local_agent_lifecycle_overrides WHERE namespace=@NS@ AND id=?1",
            ),
            [&old.id],
        )?;
        tx.execute(
            &sql(
                ns,
                "INSERT INTO local_agent_lifecycle_claims VALUES(@NS@,?1,?2,?3,?4,X'',0,NULL)
          ON CONFLICT(namespace,id) DO UPDATE SET record=NULL,request=0",
            ),
            params![old.id, old.subject, old.kind, old.origin],
        )?;
        if old.subject.starts_with("agent/") {
            dirty.insert(old.subject.clone());
        }
    }
    if let Some((c, key)) = new {
        anyhow::ensure!(
            key.5 == c.id && key.0 == c.accepted_at_unix_ms && key.3 == c.batch_id,
            "lifecycle canonical key identity mismatch"
        );
        dirty.extend(wake(tx, ns, "claim", &c.id)?);
        tx.execute(&sql(ns,"INSERT INTO local_agent_lifecycle_claims VALUES(@NS@,?1,?2,?3,?4,?5,?6,?7)
          ON CONFLICT(namespace,id) DO UPDATE SET agent=excluded.agent,kind=excluded.kind,
          origin=excluded.origin,rank=excluded.rank,request=excluded.request,record=excluded.record"),
          params![c.id,c.subject,c.kind,c.origin,canonical::sortable_key(key),is_request(c),serde_json::to_string(c)?])?;
        tx.execute(
            &sql(
                ns,
                "DELETE FROM local_agent_lifecycle_overrides WHERE namespace=@NS@ AND id=?1",
            ),
            [&c.id],
        )?;
        if c.predecessors.len() > BOUND {
            fence(
                tx,
                ns,
                &c.subject,
                "lifecycle source predecessor array exceeds bound",
            )?;
        }
        if c.kind == crate::placement::SOURCE_OFFLINE_KIND
            && c.actor
                .as_deref()
                .is_some_and(|a| a.starts_with("person/") || a.starts_with("agent/"))
            && let (Some(token), Some(destination), Some(sources)) = (
                pfield(c, "desired_token"),
                pfield(c, "destination"),
                c.body["fields"]["sources"].as_array(),
            )
        {
            if sources.len() > BOUND {
                fence(
                    tx,
                    ns,
                    &c.subject,
                    "placement override source array exceeds bound",
                )?;
            } else {
                for source in sources.iter().filter_map(Value::as_str) {
                    tx.execute(&sql(ns,"INSERT INTO local_agent_lifecycle_overrides VALUES(@NS@,?1,?2,?3,?4,?5) ON CONFLICT DO NOTHING"),params![c.subject,token,destination,source,c.id])?;
                }
            }
        }
        if c.subject.starts_with("agent/") {
            dirty.insert(c.subject.clone());
        }
    }
    for agent in &dirty {
        queue(tx, ns, agent)?;
    }
    Ok(dirty)
}

/// Known source absence (including a checkpoint tombstone) differs from uncaptured input.
/// Placement and launch reducers use live claims only; they do not follow tombstone links.
pub(crate) fn apply_absence(
    tx: &Transaction<'_>,
    ns: &Namespace,
    id: &str,
) -> Result<BTreeSet<String>> {
    let mut dirty = wake(tx, ns, "claim", id)?;
    tx.execute(
        &sql(
            ns,
            "DELETE FROM local_agent_lifecycle_overrides WHERE namespace=@NS@ AND id=?1",
        ),
        [id],
    )?;
    let prior: Option<String> = tx
        .query_row(
            &sql(
                ns,
                "SELECT agent FROM local_agent_lifecycle_claims WHERE namespace=@NS@ AND id=?1",
            ),
            [id],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(agent) = prior.filter(|a| a.starts_with("agent/")) {
        dirty.insert(agent);
    }
    tx.execute(
        &sql(
            ns,
            "INSERT INTO local_agent_lifecycle_claims VALUES(@NS@,?1,'','','',X'',0,NULL)
      ON CONFLICT(namespace,id) DO UPDATE SET record=NULL,request=0",
        ),
        [id],
    )?;
    for agent in &dirty {
        queue(tx, ns, agent)?;
    }
    Ok(dirty)
}

#[derive(Clone, Debug)]
pub(crate) struct Desired {
    pub token: String,
    pub kind: String,
}
pub(crate) fn apply_desired(
    tx: &Transaction<'_>,
    ns: &Namespace,
    agent: &str,
    desired: Option<&Desired>,
) -> Result<BTreeSet<String>> {
    tx.execute(
        &sql(
            ns,
            "INSERT INTO local_agent_lifecycle_desired VALUES(@NS@,?1,?2,?3)
      ON CONFLICT(namespace,agent) DO UPDATE SET token=excluded.token,kind=excluded.kind",
        ),
        params![agent, desired.map(|d| &d.token), desired.map(|d| &d.kind)],
    )?;
    queue(tx, ns, agent)?;
    Ok(BTreeSet::from([agent.to_owned()]))
}

/// `id` is operations' actual PK, not the human idempotency key. `selected` must be the
/// captured active canonical claim or explicit None for inactive/conflicted/missing state.
pub(crate) fn apply_operation(
    tx: &Transaction<'_>,
    ns: &Namespace,
    id: &str,
    selected: Option<&ClaimRecord>,
) -> Result<BTreeSet<String>> {
    tx.execute(
        &sql(
            ns,
            "INSERT INTO local_agent_lifecycle_operations VALUES(@NS@,?1,?2)
      ON CONFLICT(namespace,id) DO UPDATE SET record=excluded.record",
        ),
        params![id, selected.map(serde_json::to_string).transpose()?],
    )?;
    wake(tx, ns, "operation", id)
}

struct Input<'a, 'c> {
    tx: &'a Transaction<'c>,
    ns: &'a Namespace,
    agent: &'a str,
    visited: BTreeSet<String>,
}
impl Input<'_, '_> {
    fn claim(&mut self, id: &str) -> Result<Option<ClaimRecord>> {
        self.visited.insert(id.to_owned());
        anyhow::ensure!(
            self.visited.len() <= BOUND,
            "lifecycle indexed causal repair exceeds visit bound"
        );
        depend(self.tx, self.ns, self.agent, "claim", id)?;
        let value: Option<Option<String>>=self.tx.query_row(&sql(self.ns,"SELECT record FROM local_agent_lifecycle_claims WHERE namespace=@NS@ AND id=?1"),[id],|r|r.get(0)).optional()?;
        let value = value.ok_or_else(|| Need::Claim(id.to_owned()))?;
        let claim: Option<ClaimRecord> = value
            .map(|v| serde_json::from_str(&v).map_err(anyhow::Error::from))
            .transpose()?;
        if let Some(claim) = &claim {
            anyhow::ensure!(
                claim.predecessors.len() <= BOUND,
                "lifecycle source predecessor array exceeds bound"
            );
        }
        Ok(claim)
    }
    fn operation(&self, key: &str) -> Result<Option<ClaimRecord>> {
        let id = operation_id_for_key(key);
        depend(self.tx, self.ns, self.agent, "operation", &id)?;
        let value: Option<Option<String>>=self.tx.query_row(&sql(self.ns,"SELECT record FROM local_agent_lifecycle_operations WHERE namespace=@NS@ AND id=?1"),[&id],|r|r.get(0)).optional()?;
        value
            .ok_or(Need::Operation(id))?
            .map(|v| serde_json::from_str(&v).map_err(Into::into))
            .transpose()
    }
    fn runtime(&mut self, origin: &str) -> Result<Option<ClaimRecord>> {
        let id: Option<String>=self.tx.query_row(&sql(self.ns,"SELECT id FROM local_agent_lifecycle_claims
          WHERE namespace=@NS@ AND agent=?1 AND kind='runtime.observed' AND origin=?2 AND record IS NOT NULL
          ORDER BY rank DESC LIMIT 1"),params![self.agent,origin],|r|r.get(0)).optional()?;
        id.map(|id| self.claim(&id))
            .transpose()
            .map(Option::flatten)
    }
    fn latest_request(&mut self) -> Result<Option<ClaimRecord>> {
        let id: Option<String> = self
            .tx
            .query_row(
                &sql(
                    self.ns,
                    "SELECT id FROM local_agent_lifecycle_claims
          WHERE namespace=@NS@ AND agent=?1 AND request=1 ORDER BY rank DESC LIMIT 1",
                ),
                [self.agent],
                |r| r.get(0),
            )
            .optional()?;
        id.map(|id| self.claim(&id))
            .transpose()
            .map(Option::flatten)
    }
}

fn placement_fence(
    input: &mut Input<'_, '_>,
    token: &str,
) -> Result<Option<crate::placement::Fence>> {
    let Some(current) = input.claim(token)? else {
        return Ok(None);
    };
    let desired: DesiredSubject = serde_json::from_value(current.body.clone())?;
    let Some(member) = desired.member else {
        return Ok(None);
    };
    let mut result = crate::placement::Fence {
        token: token.into(),
        destination: member.host.clone(),
        sources: BTreeMap::new(),
    };
    let mut pending = vec![(current, BTreeSet::new(), BTreeSet::new())];
    let mut paths = 0;
    while let Some((claim, newer, mut hosts)) = pending.pop() {
        paths += 1;
        anyhow::ensure!(
            paths <= BOUND,
            "placement causal path expansion exceeds bound"
        );
        let desired: DesiredSubject = serde_json::from_value(claim.body.clone())?;
        if let Some(member) = desired.member {
            let host = member.host;
            if host != result.destination && hosts.insert(host.clone()) {
                result
                    .sources
                    .entry(host)
                    .and_modify(|tokens| *tokens = tokens.intersection(&newer).cloned().collect())
                    .or_insert_with(|| newer.clone());
            }
        }
        let mut newer = newer;
        newer.insert(claim.id.clone());
        anyhow::ensure!(
            newer.len() <= BOUND && result.sources.len() <= BOUND,
            "placement source/token set exceeds bound"
        );
        for parent in claim.predecessors {
            if newer.contains(&parent) {
                continue;
            }
            if let Some(previous) = input.claim(&parent)?
                && previous.subject == input.agent
                && previous.kind == "intent.desired"
            {
                anyhow::ensure!(
                    pending.len() < BOUND,
                    "placement pending causal path bound exceeded"
                );
                pending.push((previous, newer.clone(), hosts.clone()));
            }
        }
    }
    Ok(Some(result))
}
fn acknowledges(
    input: &mut Input<'_, '_>,
    claim: ClaimRecord,
    tokens: &BTreeSet<String>,
) -> Result<bool> {
    let mut pending = vec![claim];
    let mut seen = BTreeSet::new();
    while let Some(claim) = pending.pop() {
        if !seen.insert(claim.id.clone()) {
            continue;
        }
        anyhow::ensure!(
            seen.len() <= BOUND,
            "placement acknowledgement ancestry exceeds bound"
        );
        if tokens.contains(&claim.id)
            || claim
                .body
                .get("evidence")
                .and_then(Value::as_array)
                .is_some_and(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .any(|id| tokens.contains(id))
                })
        {
            return Ok(true);
        }
        for id in claim.predecessors {
            if let Some(previous) = input.claim(&id)?
                && previous.subject == input.agent
            {
                anyhow::ensure!(
                    pending.len() < BOUND,
                    "placement pending acknowledgement bound exceeded"
                );
                pending.push(previous);
            }
        }
    }
    Ok(false)
}
fn handoff(input: &mut Input<'_, '_>, token: &str) -> Result<Option<Value>> {
    let Some(f) = placement_fence(input, token)? else {
        return Ok(None);
    };
    if f.sources.is_empty() {
        return Ok(None);
    }
    let mut overridden = BTreeSet::new();
    for source in f.sources.keys() {
        let found: bool = input.tx.query_row(
            &sql(
                input.ns,
                "SELECT EXISTS(SELECT 1 FROM local_agent_lifecycle_overrides
          WHERE namespace=@NS@ AND agent=?1 AND token=?2 AND destination=?3 AND source=?4)",
            ),
            params![input.agent, f.token, f.destination, source],
            |r| r.get(0),
        )?;
        if found {
            overridden.insert(source.clone());
        }
    }
    let mut pending = Vec::new();
    for (host, tokens) in &f.sources {
        let acknowledged = match input.runtime(host)? {
            Some(c) if pfield(&c, "status") == Some("stopped") => acknowledges(input, c, tokens)?,
            _ => false,
        };
        if !acknowledged && !overridden.contains(host) {
            pending.push(host.clone());
        }
    }
    let destination = input.runtime(&f.destination)?;
    let phase = if !pending.is_empty() {
        "stopping-source"
    } else if destination
        .as_ref()
        .is_some_and(|c| pfield(c, "status") == Some("running"))
    {
        "running"
    } else if destination
        .as_ref()
        .is_some_and(|c| pfield(c, "status") == Some("starting"))
    {
        "starting"
    } else {
        "waiting-for-destination"
    };
    Ok(Some(serde_json::to_value(crate::placement::Handoff {
        phase,
        destination: f.destination,
        sources: f.sources.into_keys().collect(),
        pending_sources: pending,
        overridden_sources: overridden.into_iter().collect(),
        desired_token: f.token,
    })?))
}

fn evidence(c: &ClaimRecord, index: usize) -> Option<&str> {
    c.body
        .get("evidence")
        .and_then(|v| v.get(index))
        .and_then(Value::as_str)
}
fn failure(state: &mut Suspension, c: &ClaimRecord) {
    state.code = sfield(c, "code").map(str::to_owned);
    state.reason = sfield(c, "reason").map(str::to_owned);
    state.blocking = c
        .body
        .pointer("/fields/blocking")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    state.updated_at_unix_ms = c.accepted_at_unix_ms;
}
fn suspend_state(input: &Input<'_, '_>, request: &ClaimRecord) -> Result<Suspension> {
    let mut state = Suspension {
        action: "suspend".into(),
        phase: "quiescing".into(),
        operation_id: request.id.clone(),
        requested_by: request.actor.clone(),
        suspend_operation_id: Some(request.id.clone()),
        incarnation_id: sfield(request, "incarnation_id").map(str::to_owned),
        updated_at_unix_ms: request.accepted_at_unix_ms,
        requested_at_unix_ms: request.accepted_at_unix_ms,
        ..Suspension::default()
    };
    if let Some(c) = input.operation(&crate::suspension::suspend_failed_key(&request.id))? {
        state.phase = "failed".into();
        failure(&mut state, &c);
        return Ok(state);
    }
    if let Some(c) = input.operation(&crate::suspension::suspend_snapshot_key(&request.id))? {
        state.phase = "snapshotting".into();
        state.harness = sfield(&c, "harness").map(str::to_owned);
        state.native_session_id = sfield(&c, "native_session_id").map(str::to_owned);
        state.source_host = sfield(&c, "source_host").map(str::to_owned);
        state.updated_at_unix_ms = c.accepted_at_unix_ms;
    }
    if let Some(c) = input.operation(&crate::suspension::suspend_completed_key(&request.id))? {
        state.phase = "suspended".into();
        state.suspended_at_unix_ms = Some(c.accepted_at_unix_ms);
        state.updated_at_unix_ms = c.accepted_at_unix_ms;
    }
    Ok(state)
}
fn lineage(input: &mut Input<'_, '_>, token: &str) -> Result<Vec<String>> {
    let mut line = vec![token.to_owned()];
    let mut current = token.to_owned();
    while let Some(c) = input.claim(&current)? {
        let mut next = None;
        for parent in &c.predecessors {
            if line.contains(parent) {
                continue;
            }
            if let Some(previous) = input.claim(parent)?
                && previous.kind == "intent.desired"
                && previous.subject == input.agent
                && presentation_only_change(&previous.body, &c.body)
            {
                next = Some(previous.id);
                break;
            }
        }
        let Some(next) = next else {
            break;
        };
        anyhow::ensure!(line.len() < BOUND, "launch lineage exceeds bound");
        current = next.clone();
        line.push(next);
    }
    Ok(line)
}
fn suspension(input: &mut Input<'_, '_>, desired: Option<&Desired>) -> Result<Option<Suspension>> {
    let Some(request) = input.latest_request()? else {
        return Ok(None);
    };
    let Some(desired) = desired.filter(|d| d.kind == "agent") else {
        return Ok(None);
    };
    let line = lineage(input, &desired.token)?;
    let mut moved = false;
    if sfield(&request, "action") == Some("resume") && sfield(&request, "host").is_some() {
        for token in &line {
            if input.claim(token)?.is_some_and(|c| {
                c.body
                    .get("evidence")
                    .and_then(Value::as_array)
                    .is_some_and(|items| items.iter().any(|id| id.as_str() == Some(&request.id)))
            }) {
                moved = true;
                break;
            }
        }
    }
    if !moved && !evidence(&request, 0).is_some_and(|id| line.iter().any(|v| v == id)) {
        return Ok(None);
    }
    if sfield(&request, "action") == Some("suspend") {
        return suspend_state(input, &request).map(Some);
    }
    let Some(id) = evidence(&request, 1) else {
        return Ok(None);
    };
    let Some(suspend) = input
        .claim(id)?
        .filter(|c| c.subject == input.agent && is_request(c))
    else {
        return Ok(None);
    };
    let mut state = suspend_state(input, &suspend)?;
    if state.phase != "suspended" {
        return Ok(None);
    }
    state.action = "resume".into();
    state.operation_id = request.id.clone();
    state.requested_by = request.actor.clone();
    state.updated_at_unix_ms = request.accepted_at_unix_ms;
    state.requested_at_unix_ms = request.accepted_at_unix_ms;
    state.host = sfield(&request, "host").map(str::to_owned);
    state.source_host = sfield(&request, "source_host")
        .map(str::to_owned)
        .or(state.source_host);
    state.phase = if state.host.is_some() {
        "fencing-source"
    } else {
        "restoring"
    }
    .into();
    for (suffix, phase) in [("transfer", "transferring"), ("restored", "restoring")] {
        if let Some(c) = input.operation(&format!("agent-resume-{suffix}:{}", request.id))? {
            state.phase = phase.into();
            state.updated_at_unix_ms = c.accepted_at_unix_ms;
        }
    }
    if moved && matches!(state.phase.as_str(), "fencing-source" | "transferring") {
        state.phase = "restoring".into();
    }
    if let Some(c) = input.operation(&crate::suspension::resume_failed_key(&request.id))? {
        state.phase = "suspended".into();
        failure(&mut state, &c);
        return Ok(Some(state));
    }
    if let Some(c) = input.operation(&crate::suspension::resume_started_key(&request.id))? {
        state.phase = "verifying".into();
        state.updated_at_unix_ms = c.accepted_at_unix_ms;
    }
    if let Some(c) = input.operation(&crate::suspension::resume_completed_key(&request.id))? {
        state.phase = "resumed".into();
        state.incarnation_id = sfield(&c, "incarnation_id").map(str::to_owned);
        state.updated_at_unix_ms = c.accepted_at_unix_ms;
    }
    Ok(Some(state))
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Lifecycle {
    pub desired_token: Option<String>,
    pub handoff: Option<Value>,
    pub suspension: Option<Suspension>,
}
fn repair(tx: &Transaction<'_>, ns: &Namespace, agent: &str) -> Result<Lifecycle> {
    tx.execute(
        &sql(
            ns,
            "DELETE FROM local_agent_lifecycle_dependencies WHERE namespace=@NS@ AND agent=?1",
        ),
        [agent],
    )?;
    let selected: Option<(Option<String>,Option<String>)>=tx.query_row(&sql(ns,"SELECT token,kind FROM local_agent_lifecycle_desired WHERE namespace=@NS@ AND agent=?1"),[agent],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    let (token, kind) = selected.ok_or_else(|| Need::Desired(agent.to_owned()))?;
    let desired = token.zip(kind).map(|(token, kind)| Desired { token, kind });
    let mut input = Input {
        tx,
        ns,
        agent,
        visited: BTreeSet::new(),
    };
    let handoff = desired
        .as_ref()
        .map(|d| handoff(&mut input, &d.token))
        .transpose()?
        .flatten();
    let suspension = suspension(&mut input, desired.as_ref())?;
    Ok(Lifecycle {
        desired_token: desired.map(|d| d.token),
        handoff,
        suspension,
    })
}
/// Advance `after` through one bounded dirty scan even when an earlier agent is pending.
/// `missing` names exact lookups to capture at the certified source cut, never a later live
/// Store read. `complete` covers this namespace dependency only. After reaching the end,
/// a maintenance sweep starts again at the empty cursor to revisit deferred rows.
pub(crate) struct RepairPage {
    pub after: Option<String>,
    pub changed: BTreeSet<String>,
    pub missing: BTreeSet<Need>,
    pub complete: bool,
}
pub(crate) fn flush_page(
    tx: &Transaction<'_>,
    ns: &Namespace,
    after: &str,
    limit: usize,
) -> Result<RepairPage> {
    anyhow::ensure!(
        (1..=128).contains(&limit),
        "lifecycle repair page exceeds bound"
    );
    let agents=tx.prepare_cached(&sql(ns,"SELECT agent FROM local_agent_lifecycle_dirty WHERE namespace=@NS@ AND agent>?1 ORDER BY agent LIMIT ?2"))?
      .query_map(params![after,limit],|r|r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    let mut page = RepairPage {
        after: agents.last().cloned(),
        changed: BTreeSet::new(),
        missing: BTreeSet::new(),
        complete: false,
    };
    for agent in agents {
        match repair(tx, ns, &agent) {
            Ok(row) => {
                let body = serde_json::to_string(&row)?;
                let old: Option<String>=tx.query_row(&sql(ns,"SELECT body FROM local_agent_lifecycle_rows WHERE namespace=@NS@ AND agent=?1"),[&agent],|r|r.get(0)).optional()?;
                if old.as_deref() != Some(&body) {
                    tx.execute(&sql(ns,"INSERT INTO local_agent_lifecycle_rows VALUES(@NS@,?1,?2) ON CONFLICT(namespace,agent) DO UPDATE SET body=excluded.body"),params![agent,body])?;
                    page.changed.insert(agent.clone());
                }
                tx.execute(
                    &sql(
                        ns,
                        "DELETE FROM local_agent_lifecycle_dirty WHERE namespace=@NS@ AND agent=?1",
                    ),
                    [&agent],
                )?;
            }
            Err(e) => {
                if let Some(need) = e.downcast_ref::<Need>() {
                    page.missing.insert(need.clone());
                } else if e.chain().any(|e| e.is::<rusqlite::Error>()) {
                    return Err(e);
                } else {
                    fence(tx, ns, &agent, &format!("{e:#}"))?;
                }
            }
        }
    }
    page.complete = !pending(tx, ns)?;
    Ok(page)
}
fn pending(c: &Connection, ns: &Namespace) -> Result<bool> {
    Ok(c.query_row(
        &sql(
            ns,
            "SELECT EXISTS(SELECT 1 FROM local_agent_lifecycle_dirty WHERE namespace=@NS@)
      OR EXISTS(SELECT 1 FROM local_agent_lifecycle_fences WHERE namespace=@NS@)",
        ),
        [],
        |r| r.get(0),
    )?)
}
/// Dependency closure only. The unified owner must additionally prove the full source cut,
/// authority and every other card dependency before publishing a Ready namespace.
pub(crate) fn ensure_closed(c: &Connection, ns: &Namespace) -> Result<()> {
    anyhow::ensure!(!pending(c, ns)?, "lifecycle namespace pending or fenced");
    Ok(())
}
/// Read only in an authorized snapshot with the unified full-card/source certificate.
/// The indexed pending guard here cannot prove authority or omitted family coverage.
pub(crate) fn read_lifecycle(c: &Connection, ns: &Namespace, agent: &str) -> Result<Lifecycle> {
    let pending: bool=c.query_row(&sql(ns,"SELECT EXISTS(SELECT 1 FROM local_agent_lifecycle_dirty WHERE namespace=@NS@ AND agent=?1)
      OR EXISTS(SELECT 1 FROM local_agent_lifecycle_fences WHERE namespace=@NS@ AND agent=?1)"),[agent],|r|r.get(0))?;
    anyhow::ensure!(!pending, "agent lifecycle pending or fenced");
    let body: String = c.query_row(
        &sql(
            ns,
            "SELECT body FROM local_agent_lifecycle_rows WHERE namespace=@NS@ AND agent=?1",
        ),
        [agent],
        |r| r.get(0),
    )?;
    Ok(serde_json::from_str(&body)?)
}

/// A total row budget across this dependency's tables; the unified operator must also
/// share its aggregate reclamation budget with its other dependencies.
pub(crate) fn reclaim_namespace(
    tx: &Transaction<'_>,
    ns: &Namespace,
    limit: usize,
) -> Result<bool> {
    anyhow::ensure!(
        (1..=128).contains(&limit),
        "lifecycle reclamation page exceeds bound"
    );
    let ready: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM ivm_install_roots WHERE namespace=?1 AND ready=1)",
        [ns.as_str()],
        |r| r.get(0),
    )?;
    anyhow::ensure!(!ready, "cannot reclaim a ready lifecycle namespace");
    let tables = [
        ("local_agent_lifecycle_claims", "id"),
        (
            "local_agent_lifecycle_overrides",
            "agent,token,destination,source,id",
        ),
        ("local_agent_lifecycle_desired", "agent"),
        ("local_agent_lifecycle_operations", "id"),
        ("local_agent_lifecycle_dependencies", "kind,key,agent"),
        ("local_agent_lifecycle_rows", "agent"),
        ("local_agent_lifecycle_dirty", "agent"),
        ("local_agent_lifecycle_fences", "agent"),
    ];
    let mut remaining = limit;
    for (table, keys) in tables {
        if remaining == 0 {
            break;
        }
        let query = format!(
            "DELETE FROM {table} WHERE namespace=@NS@ AND ({keys}) IN (SELECT {keys} FROM {table} WHERE namespace=@NS@ ORDER BY {keys} LIMIT ?1)"
        );
        remaining -= tx.execute(&sql(ns, &query), [remaining])?;
    }
    for (table, _) in tables {
        if tx.query_row(
            &sql(
                ns,
                &format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE namespace=@NS@)"),
            ),
            [],
            |r| r.get::<_, bool>(0),
        )? {
            return Ok(false);
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests;
