//! Namespaced todo/session/fault heads and subagent dependencies for the complete agent card.
//!
//! Caller admits only inputs through its certified snapshot cut, supplies captured canonical keys,
//! and redispatches checkpoint index/rank renumbering as old/new replacements. This module neither
//! installs a registry nor certifies a partial card. All getters require the namespace to remain
//! complete; pending/exhausted output is refused. Maintenance reads at most 65 appearances per ID,
//! updates at most four affected IDs, and current arrays contain at most 256 non-ended appearances.
//! `Effects::unavailable` requires the owner to fence the complete card and record install diagnosis;
//! `coverage` must be checked even on silent collection advances. Empty private tables do not prove
//! backfill completeness. Reads have no historical-cut argument: the namespace must contain exactly
//! the owner's independently certified admitted cut. The owner supplies timer wake/commit at
//! `next_deadline` (expiry is inclusive), and admits every claim replacement/deletion and checkpoint
//! rank/arrival renumbering. Unsupported metadata permanently refuses that agent in this namespace;
//! recovery requires a fresh complete namespace. Nodes retain history; no production memory or
//! callback-cost bound is certified by these fixed output/work limits.

use super::SubagentView;
use anyhow::{Result, anyhow, ensure};
use rusqlite::{Connection, OptionalExtension, Transaction, params, types::Value as SqlValue};
use serde_json::Value;
use smallclaims::{
    ClaimRecord,
    ivm::install::Namespace,
    store::canonical::{self, ClaimKey},
};
use std::collections::BTreeSet;

pub(crate) const APPEARANCES_PER_ID: usize = 64;
pub(crate) const OPEN_ROWS_PER_AGENT: usize = 256;
pub(crate) const RECORD_BYTES: usize = 64 * 1024;
const KINDS: &[&str] = &[
    "harness.todo.observed",
    "harness.session-file",
    "runtime.reconcile-decision",
    "subagent.appeared",
    "subagent.renewed",
    "subagent.ended",
];

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS local_agent_card_aux_nodes(
 namespace TEXT NOT NULL,claim TEXT NOT NULL,agent TEXT NOT NULL,kind TEXT NOT NULL,
 rank BLOB NOT NULL,arrival INTEGER NOT NULL,record TEXT NOT NULL,subid,
 lease_cast INTEGER,fault_eligible INTEGER NOT NULL,
 PRIMARY KEY(namespace,claim)
);
CREATE INDEX IF NOT EXISTS local_agent_card_aux_canonical ON local_agent_card_aux_nodes(namespace,agent,kind,rank DESC);
CREATE INDEX IF NOT EXISTS local_agent_card_aux_arrival ON local_agent_card_aux_nodes(namespace,agent,kind,fault_eligible,arrival DESC);
CREATE INDEX IF NOT EXISTS local_agent_card_aux_group_rank ON local_agent_card_aux_nodes(namespace,agent,subid,kind,rank);
CREATE INDEX IF NOT EXISTS local_agent_card_aux_group_lease ON local_agent_card_aux_nodes(namespace,agent,subid,kind,lease_cast DESC);
CREATE TABLE IF NOT EXISTS local_agent_card_aux_agents(
 namespace TEXT NOT NULL,agent TEXT NOT NULL,pending INTEGER NOT NULL DEFAULT 0,
 rows INTEGER NOT NULL DEFAULT 0,exhausted_groups INTEGER NOT NULL DEFAULT 0,
 unsupported INTEGER NOT NULL DEFAULT 0,PRIMARY KEY(namespace,agent)
);
CREATE TABLE IF NOT EXISTS local_agent_card_aux_groups(
 namespace TEXT NOT NULL,agent TEXT NOT NULL,subid TEXT NOT NULL,
 rows INTEGER NOT NULL,exhausted INTEGER NOT NULL,PRIMARY KEY(namespace,agent,subid)
);
CREATE TABLE IF NOT EXISTS local_agent_card_aux_subagents(
 namespace TEXT NOT NULL,agent TEXT NOT NULL,subid TEXT NOT NULL,claim TEXT NOT NULL,
 rank BLOB NOT NULL,record TEXT NOT NULL,renewed INTEGER NOT NULL,expires INTEGER NOT NULL,
 PRIMARY KEY(namespace,claim)
);
CREATE INDEX IF NOT EXISTS local_agent_card_aux_subagents_group ON local_agent_card_aux_subagents(namespace,agent,subid);
CREATE INDEX IF NOT EXISTS local_agent_card_aux_subagents_due ON local_agent_card_aux_subagents(namespace,agent,expires,rank);
"#;

pub(crate) fn create_schema(connection: &Connection) -> Result<()> {
    connection.execute_batch(SCHEMA)?;
    Ok(())
}

#[derive(Debug, Default)]
pub(crate) struct Effects {
    pub(crate) agents: BTreeSet<String>,
    pub(crate) unavailable: BTreeSet<String>,
}

pub(crate) fn apply_claim(
    tx: &Transaction<'_>,
    namespace: &Namespace,
    old: Option<&ClaimRecord>,
    new: Option<(&ClaimRecord, &ClaimKey)>,
) -> Result<Effects> {
    apply(tx, namespace.as_str(), old, new)
}

fn wanted(claim: &ClaimRecord) -> bool {
    claim.subject.starts_with("agent/") && KINDS.contains(&claim.kind.as_str())
}
fn subkind(kind: &str) -> bool {
    kind.starts_with("subagent.")
}

fn subject_id(claim: &ClaimRecord, tx: &Transaction<'_>) -> Result<Option<String>> {
    let subid: SqlValue = tx.query_row(
        "SELECT json_extract(?1,'$.fields.subagent_id')",
        [claim.body.to_string()],
        |r| r.get(0),
    )?;
    // json_extract arrays/objects are SQLite TEXT and can match a literal string ID. Numeric
    // IDs do not match text IDs in the legacy comparison; do not add column affinity coercion.
    Ok(match subid {
        SqlValue::Text(id) if !id.is_empty() => Some(id),
        _ => None,
    })
}

fn touch(tx: &Transaction<'_>, namespace: &str, agent: &str) -> Result<()> {
    tx.execute("INSERT INTO local_agent_card_aux_agents(namespace,agent,pending) VALUES(?1,?2,1) ON CONFLICT(namespace,agent) DO UPDATE SET pending=1",params![namespace,agent])?;
    Ok(())
}

fn apply(
    tx: &Transaction<'_>,
    namespace: &str,
    old: Option<&ClaimRecord>,
    new: Option<(&ClaimRecord, &ClaimKey)>,
) -> Result<Effects> {
    let mut effects = Effects::default();
    let mut groups = BTreeSet::new();
    for claim in old.into_iter().chain(new.map(|(claim, _)| claim)) {
        let staged: Option<(String,String,SqlValue)> = tx.query_row("SELECT agent,kind,subid FROM local_agent_card_aux_nodes WHERE namespace=?1 AND claim=?2",params![namespace,claim.id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
        if let Some((agent, kind, subid)) = staged {
            effects.agents.insert(agent.clone());
            if subkind(&kind)
                && let SqlValue::Text(id) = subid
                && !id.is_empty()
            {
                groups.insert((agent, id));
            }
        }
        if wanted(claim) {
            effects.agents.insert(claim.subject.clone());
            if subkind(&claim.kind)
                && let Some(id) = subject_id(claim, tx)?
            {
                groups.insert((claim.subject.clone(), id));
            }
        }
    }
    ensure!(
        groups.len() <= 4 && effects.agents.len() <= 4,
        "aux affected-key bound exceeded"
    );
    for agent in &effects.agents {
        touch(tx, namespace, agent)?;
    }
    for claim in old.into_iter().chain(new.map(|(claim, _)| claim)) {
        tx.execute(
            "DELETE FROM local_agent_card_aux_nodes WHERE namespace=?1 AND claim=?2",
            params![namespace, claim.id],
        )?;
    }
    if let Some((claim, key)) = new.filter(|(claim, _)| wanted(claim)) {
        let record = serde_json::to_string(claim)?;
        let rank = canonical::sortable_key(key);
        let bounded = record.len() <= RECORD_BYTES
            && rank.len() <= 4096
            && claim.subject.len() <= 1024
            && claim.id.len() <= 1024
            && claim.store_index <= i64::MAX as u64
            && key.0 == claim.accepted_at_unix_ms
            && key.3 == claim.batch_id
            && key.5 == claim.id
            && groups.iter().all(|(_, id)| id.len() <= 1024);
        if bounded {
            tx.execute("INSERT INTO local_agent_card_aux_nodes VALUES(?1,?2,?3,?4,?5,?6,?7,json_extract(?8,'$.fields.subagent_id'),CAST(json_extract(?8,'$.fields.lease_expires_at_unix_ms') AS INTEGER),CASE WHEN json_extract(?8,'$.fields.key')='member-reconcile' THEN 1 ELSE 0 END)",params![namespace,claim.id,claim.subject,claim.kind,rank,claim.store_index,record,claim.body.to_string()])?;
        } else {
            // Persist the refusal while allowing the owning adapter to fence the complete view.
            // No truncation or partial current row may be returned; fresh namespace recovery needed.
            tx.execute("UPDATE local_agent_card_aux_agents SET unsupported=1 WHERE namespace=?1 AND agent=?2",params![namespace,claim.subject])?;
        }
    }
    for (agent, id) in groups {
        recompute_group(tx, namespace, &agent, &id)?;
    }
    for agent in &effects.agents {
        tx.execute(
            "UPDATE local_agent_card_aux_agents SET pending=0 WHERE namespace=?1 AND agent=?2",
            params![namespace, agent],
        )?;
        if !available(tx, namespace, agent)? {
            effects.unavailable.insert(agent.clone());
        }
    }
    Ok(effects)
}

fn recompute_group(tx: &Transaction<'_>, namespace: &str, agent: &str, id: &str) -> Result<()> {
    let prior: (usize,usize)=tx.query_row("SELECT rows,exhausted FROM local_agent_card_aux_groups WHERE namespace=?1 AND agent=?2 AND subid=?3",params![namespace,agent,id],|r|Ok((r.get(0)?,r.get(1)?))).optional()?.unwrap_or((0,0));
    tx.execute(
        "DELETE FROM local_agent_card_aux_subagents WHERE namespace=?1 AND agent=?2 AND subid=?3",
        params![namespace, agent, id],
    )?;
    let ended=tx.query_row("SELECT 1 FROM local_agent_card_aux_nodes WHERE namespace=?1 AND agent=?2 AND subid=?3 AND kind='subagent.ended' LIMIT 1",params![namespace,agent,id],|_|Ok(true)).optional()?.unwrap_or(false);
    let mut count = 0;
    let mut exhausted = 0;
    if !ended {
        let mut statement=tx.prepare_cached("SELECT rank,record,claim FROM local_agent_card_aux_nodes WHERE namespace=?1 AND agent=?2 AND subid=?3 AND kind='subagent.appeared' ORDER BY rank LIMIT ?4")?;
        let rows = statement
            .query_map(params![namespace, agent, id, APPEARANCES_PER_ID + 1], |r| {
                Ok((
                    r.get::<_, Vec<u8>>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if rows.len() > APPEARANCES_PER_ID {
            exhausted = 1;
        } else {
            let max_lease = |kind: &str| -> Result<Option<i64>> {
                Ok(tx.query_row("SELECT lease_cast FROM local_agent_card_aux_nodes WHERE namespace=?1 AND agent=?2 AND subid=?3 AND kind=?4 ORDER BY lease_cast DESC LIMIT 1",params![namespace,agent,id,kind],|r|r.get::<_,Option<i64>>(0)).optional()?.flatten())
            };
            let renewed = max_lease("subagent.renewed")?.unwrap_or(0).max(0);
            let live = max_lease("subagent.appeared")?
                .into_iter()
                .chain(max_lease("subagent.renewed")?)
                .max()
                .unwrap_or(0)
                .max(0) as u64;
            for (rank, record, claim) in rows {
                let parsed: ClaimRecord = serde_json::from_str(&record)?;
                let fields = &parsed.body["fields"];
                if text(fields, "subagent_id").is_none() {
                    continue;
                }
                let lease = fields["lease_expires_at_unix_ms"]
                    .as_u64()
                    .unwrap_or(0)
                    .max(renewed as u64);
                let expiry = lease.min(live);
                tx.execute(
                    "INSERT INTO local_agent_card_aux_subagents VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
                    params![namespace, agent, id, claim, rank, record, renewed, expiry],
                )?;
                count += 1;
            }
        }
    }
    tx.execute("INSERT INTO local_agent_card_aux_groups VALUES(?1,?2,?3,?4,?5) ON CONFLICT(namespace,agent,subid) DO UPDATE SET rows=excluded.rows,exhausted=excluded.exhausted",params![namespace,agent,id,count,exhausted])?;
    tx.execute("UPDATE local_agent_card_aux_agents SET rows=rows-?3+?4,exhausted_groups=exhausted_groups-?5+?6 WHERE namespace=?1 AND agent=?2",params![namespace,agent,prior.0,count,prior.1,exhausted])?;
    Ok(())
}

fn available(connection: &Connection, namespace: &str, agent: &str) -> Result<bool> {
    let flags: Option<(usize,usize,usize,usize)>=connection.query_row("SELECT pending,rows,exhausted_groups,unsupported FROM local_agent_card_aux_agents WHERE namespace=?1 AND agent=?2",params![namespace,agent],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
    Ok(flags.is_none_or(|(pending, rows, groups, unsupported)| {
        pending == 0 && rows <= OPEN_ROWS_PER_AGENT && groups == 0 && unsupported == 0
    }))
}

fn require(connection: &Connection, namespace: &str, agent: &str) -> Result<()> {
    ensure!(
        available(connection, namespace, agent)?,
        "aux source pending or exhausted"
    );
    Ok(())
}

pub(crate) fn read_todo_session(
    connection: &Connection,
    namespace: &Namespace,
    agent: &str,
) -> Result<(Option<ClaimRecord>, Option<ClaimRecord>)> {
    todo_session(connection, namespace.as_str(), agent)
}
fn todo_session(
    connection: &Connection,
    namespace: &str,
    agent: &str,
) -> Result<(Option<ClaimRecord>, Option<ClaimRecord>)> {
    require(connection, namespace, agent)?;
    let read = |kind: &str| -> Result<Option<ClaimRecord>> {
        let record: Option<String>=connection.query_row("SELECT record FROM local_agent_card_aux_nodes WHERE namespace=?1 AND agent=?2 AND kind=?3 ORDER BY rank DESC LIMIT 1",params![namespace,agent,kind],|r|r.get(0)).optional()?;
        record
            .map(|value| serde_json::from_str(&value).map_err(Into::into))
            .transpose()
    };
    Ok((
        read("harness.todo.observed")?,
        read("harness.session-file")?,
    ))
}

pub(crate) fn read_fault(
    connection: &Connection,
    namespace: &Namespace,
    agent: &str,
) -> Result<Option<String>> {
    fault(connection, namespace.as_str(), agent)
}
fn fault(connection: &Connection, namespace: &str, agent: &str) -> Result<Option<String>> {
    require(connection, namespace, agent)?;
    let record: Option<String>=connection.query_row("SELECT record FROM local_agent_card_aux_nodes WHERE namespace=?1 AND agent=?2 AND kind='runtime.reconcile-decision' AND fault_eligible=1 ORDER BY arrival DESC LIMIT 1",params![namespace,agent],|r|r.get(0)).optional()?;
    let Some(record) = record else {
        return Ok(None);
    };
    let record: ClaimRecord = serde_json::from_str(&record)?;
    let fields = record.body.get("fields").unwrap_or(&record.body);
    Ok(
        (fields["decision"].as_str() == Some("member-fault")).then(|| {
            fields["reason"]
                .as_str()
                .unwrap_or("member reconciliation failed")
                .to_owned()
        }),
    )
}

fn text(fields: &Value, name: &str) -> Option<String> {
    fields
        .get(name)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}
fn subagent(record: ClaimRecord, renewed: i64) -> Result<SubagentView> {
    let fields = &record.body["fields"];
    Ok(SubagentView {
        agent: record.subject,
        subagent_id: text(fields, "subagent_id")
            .ok_or_else(|| anyhow!("subagent cache contains invalid ID"))?,
        subagent_type: text(fields, "subagent_type"),
        description: text(fields, "description"),
        driver: text(fields, "driver").unwrap_or_default(),
        session_id: text(fields, "session_id"),
        incarnation_id: text(fields, "incarnation_id").unwrap_or_default(),
        step_run: text(fields, "step_run"),
        started_at_unix_ms: fields["started_at_unix_ms"].as_u64().unwrap_or(0),
        lease_expires_at_unix_ms: fields["lease_expires_at_unix_ms"]
            .as_u64()
            .unwrap_or(0)
            .max(renewed.max(0) as u64),
        appeared: record.id,
        origin: record.origin,
        store_index: record.store_index,
    })
}

pub(crate) fn running_subagents_for(
    connection: &Connection,
    namespace: &Namespace,
    agent: &str,
    now: u64,
) -> Result<Vec<SubagentView>> {
    running(connection, namespace.as_str(), agent, now)
}
fn running(
    connection: &Connection,
    namespace: &str,
    agent: &str,
    now: u64,
) -> Result<Vec<SubagentView>> {
    require(connection, namespace, agent)?;
    let mut statement=connection.prepare_cached("SELECT record,renewed FROM local_agent_card_aux_subagents WHERE namespace=?1 AND agent=?2 AND expires>?3 ORDER BY rank LIMIT ?4")?;
    let rows = statement
        .query_map(
            params![
                namespace,
                agent,
                now.min(i64::MAX as u64),
                OPEN_ROWS_PER_AGENT + 1
            ],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    ensure!(
        rows.len() <= OPEN_ROWS_PER_AGENT,
        "aux live output exhausted"
    );
    rows.into_iter()
        .map(|(record, renewed)| subagent(serde_json::from_str(&record)?, renewed))
        .collect()
}

pub(crate) fn next_deadline(
    connection: &Connection,
    namespace: &Namespace,
    agent: &str,
    now: u64,
) -> Result<Option<u64>> {
    deadline(connection, namespace.as_str(), agent, now)
}
fn deadline(
    connection: &Connection,
    namespace: &str,
    agent: &str,
    now: u64,
) -> Result<Option<u64>> {
    require(connection, namespace, agent)?;
    Ok(connection.query_row("SELECT expires FROM local_agent_card_aux_subagents WHERE namespace=?1 AND agent=?2 AND expires>?3 ORDER BY expires LIMIT 1",params![namespace,agent,now.min(i64::MAX as u64)],|r|r.get::<_,u64>(0)).optional()?)
}

#[cfg(test)]
#[path = "agent_card_aux/tests.rs"]
mod tests;
