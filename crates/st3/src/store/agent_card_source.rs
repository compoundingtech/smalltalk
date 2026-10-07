//! Complete namespace card composition. Source capture, graph-prefix certification,
//! installation lifetime and producer acknowledgement belong to the shared source owner.
//! Ordinary physical mutations stage work; only captured maintenance inputs drain it.
use super::collection_ivm::agent_source::{self, canonical::Fact, shadow};
use super::*;
use crate::api::delivery_presence::source::{
    Certificate as DeliveryCertificate, boundary::FileEvidence,
};
use smallclaims::ivm::install::{Mutation, Namespace, Operator, SourcePosition};

pub(crate) const FINGERPRINT: &str = "st3.agent-card.complete.v2;namespace-v2;physical-source-v5;registry-unfiltered-admission;canonical-decimal-time;original-sql-body;harness-v3-captured-since;activity-v2-native-cut-ranges128;authority-v2-parent-identity;owned-v2-relevant-members128-sets64-lineage64-claims100k-captured64m;unmanaged-leaves128-body256k;queue-v1-labels-path1024-preview5;usage-v2-f64-groups128-slots128;rollout-field-heads-v1;launch-v1-physical-ties-bounds128;aux-v1-appear64-open256-record64k;native-global-v1-files64-indexed-requests;clock-v2-snapshot-position;shared-work128-seek-cursors-public-queue-ack;window200;public-card-v0;current-state-only";
pub(crate) fn complete_manifest() -> String {
    format!(
        "{FINGERPRINT};source={}",
        agent_source::capture_fingerprint()
    )
}

const WORK: usize = 128;
const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS local_agent_card_source_facts(namespace TEXT NOT NULL,id TEXT NOT NULL,subject TEXT NOT NULL,body TEXT NOT NULL,PRIMARY KEY(namespace,id));
CREATE INDEX IF NOT EXISTS local_agent_card_source_subject ON local_agent_card_source_facts(namespace,subject,id);
CREATE TABLE IF NOT EXISTS local_agent_card_source_links(namespace TEXT NOT NULL,id TEXT NOT NULL,body TEXT NOT NULL,PRIMARY KEY(namespace,id));
CREATE TABLE IF NOT EXISTS local_agent_card_source_physical(namespace TEXT NOT NULL,key TEXT NOT NULL,body TEXT NOT NULL,PRIMARY KEY(namespace,key));
CREATE TABLE IF NOT EXISTS local_agent_card_source_cursor(namespace TEXT NOT NULL,kind TEXT NOT NULL,after_key TEXT NOT NULL,PRIMARY KEY(namespace,kind));
CREATE TABLE IF NOT EXISTS local_agent_card_source_work(namespace TEXT NOT NULL,kind TEXT NOT NULL,key TEXT NOT NULL,PRIMARY KEY(namespace,kind,key));
CREATE TABLE IF NOT EXISTS local_agent_card_source_cards(namespace TEXT NOT NULL,agent TEXT NOT NULL,deadline BLOB,files TEXT NOT NULL,mono_deadline INTEGER,certificate TEXT,PRIMARY KEY(namespace,agent));
CREATE INDEX IF NOT EXISTS local_agent_card_source_deadline ON local_agent_card_source_cards(namespace,deadline,agent) WHERE deadline IS NOT NULL;
CREATE INDEX IF NOT EXISTS local_agent_card_source_mono ON local_agent_card_source_cards(namespace,mono_deadline,agent) WHERE mono_deadline IS NOT NULL;
CREATE TABLE IF NOT EXISTS local_agent_card_source_native(namespace TEXT NOT NULL,agent TEXT NOT NULL,driver TEXT NOT NULL,epoch TEXT NOT NULL,deadline BLOB,needed INTEGER NOT NULL CHECK(needed IN (0,1)),PRIMARY KEY(namespace,agent));
CREATE INDEX IF NOT EXISTS local_agent_card_source_native_needed ON local_agent_card_source_native(namespace,needed,agent,driver);
CREATE INDEX IF NOT EXISTS local_agent_card_source_native_epoch ON local_agent_card_source_native(namespace,needed,epoch,agent,driver);
CREATE INDEX IF NOT EXISTS local_agent_card_source_native_deadline ON local_agent_card_source_native(namespace,needed,deadline,agent,driver) WHERE deadline IS NOT NULL;
CREATE TABLE IF NOT EXISTS local_agent_card_source_files(namespace TEXT NOT NULL,path TEXT NOT NULL,identity TEXT NOT NULL,count INTEGER NOT NULL CHECK(count>0),PRIMARY KEY(namespace,path,identity));
CREATE TABLE IF NOT EXISTS local_agent_card_source_reclaim(namespace TEXT PRIMARY KEY,phase INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS local_agent_card_source_clock(namespace TEXT PRIMARY KEY,at TEXT NOT NULL,revision INTEGER NOT NULL,phase INTEGER NOT NULL,authority_agent TEXT NOT NULL,authority_id TEXT NOT NULL,lifecycle_after TEXT NOT NULL,snapshot_index INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS local_agent_card_source_local_cut(namespace TEXT NOT NULL,revision INTEGER NOT NULL,lower_index INTEGER NOT NULL,upper_index INTEGER NOT NULL,after_index INTEGER NOT NULL,after_claim TEXT NOT NULL,PRIMARY KEY(namespace,revision));
"#;

pub(crate) struct Kernel {
    origin: String,
}
impl Kernel {
    pub(crate) fn new(origin: &str) -> Self {
        Self {
            origin: origin.into(),
        }
    }
}

fn queue(tx: &Transaction<'_>, ns: &Namespace, kind: &str, key: &str) -> Result<()> {
    tx.execute(
        "INSERT INTO local_agent_card_source_work VALUES(?1,?2,?3) ON CONFLICT DO NOTHING",
        params![ns.as_str(), kind, key],
    )?;
    Ok(())
}
fn cards(
    tx: &Transaction<'_>,
    ns: &Namespace,
    ids: impl IntoIterator<Item = String>,
) -> Result<()> {
    for agent in ids.into_iter().filter(|s| s.starts_with("agent/")) {
        queue(tx, ns, "card", &agent)?;
    }
    Ok(())
}
fn agents(
    tx: &Transaction<'_>,
    ns: &Namespace,
    ids: impl IntoIterator<Item = String>,
) -> Result<()> {
    for agent in ids.into_iter().filter(|s| s.starts_with("agent/")) {
        queue(tx, ns, "select", &agent)?;
        queue(tx, ns, "card", &agent)?;
    }
    Ok(())
}
fn cursor(c: &Connection, ns: &Namespace, kind: &str) -> Result<String> {
    Ok(c.query_row(
        "SELECT after_key FROM local_agent_card_source_cursor WHERE namespace=?1 AND kind=?2",
        params![ns.as_str(), kind],
        |r| r.get(0),
    )
    .optional()?
    .unwrap_or_default())
}
fn set_cursor(tx: &Transaction<'_>, ns: &Namespace, kind: &str, after: &str) -> Result<()> {
    tx.execute("INSERT INTO local_agent_card_source_cursor VALUES(?1,?2,?3) ON CONFLICT(namespace,kind) DO UPDATE SET after_key=excluded.after_key",params![ns.as_str(),kind,after])?;
    Ok(())
}
fn page(tx: &Transaction<'_>, ns: &Namespace, kind: &str, limit: usize) -> Result<Vec<String>> {
    let after = cursor(tx, ns, kind)?;
    let mut keys:Vec<String>=tx.prepare_cached("SELECT key FROM local_agent_card_source_work WHERE namespace=?1 AND kind=?2 AND key>?3 ORDER BY key LIMIT ?4")?.query_map(params![ns.as_str(),kind,after,limit+1],|r|r.get(0))?.collect::<rusqlite::Result<_>>()?;
    let more = keys.len() > limit;
    keys.truncate(limit);
    set_cursor(
        tx,
        ns,
        kind,
        if more {
            keys.last().map(String::as_str).unwrap_or("")
        } else {
            ""
        },
    )?;
    Ok(keys)
}
fn card_work(c: &Connection, ns: &Namespace) -> Result<bool> {
    Ok(c.query_row("SELECT EXISTS(SELECT 1 FROM local_agent_card_source_work WHERE namespace=?1 AND kind='card')",[ns.as_str()],|r|r.get(0))?)
}
fn queue_public_pending(c: &Connection, ns: &Namespace) -> Result<bool> {
    Ok(c.query_row(
        "SELECT EXISTS(SELECT 1 FROM local_agent_queue_dirty WHERE namespace=?1)",
        [ns.as_str()],
        |r| r.get(0),
    )?)
}

fn ack(tx: &Transaction<'_>, ns: &Namespace, kind: &str, key: &str) -> Result<()> {
    tx.execute(
        "DELETE FROM local_agent_card_source_work WHERE namespace=?1 AND kind=?2 AND key=?3",
        params![ns.as_str(), kind, key],
    )?;
    Ok(())
}
fn physical_key(table: &str, key: Value) -> Result<String> {
    Ok(serde_json::to_string(&json!([table, key]))?)
}
fn key_parts(key: &str) -> Result<(String, Vec<Value>)> {
    let (table, pk): (String, Vec<Value>) = serde_json::from_str(key)?;
    anyhow::ensure!(
        agent_source::TABLES
            .iter()
            .any(|t| t.name == table && t.key.len() == pk.len()),
        "card physical key outside manifest"
    );
    Ok((table, pk))
}
fn captured_index(c: &Connection, ns: &Namespace) -> Result<u64> {
    Ok(c.query_row(
        "SELECT snapshot_index FROM local_agent_card_source_clock WHERE namespace=?1",
        [ns.as_str()],
        |r| r.get(0),
    )?)
}

fn fact(c: &Connection, ns: &Namespace, id: &str) -> Result<Option<Fact>> {
    let body: Option<String> = c
        .query_row(
            "SELECT body FROM local_agent_card_source_facts WHERE namespace=?1 AND id=?2",
            params![ns.as_str(), id],
            |r| r.get(0),
        )
        .optional()?;
    body.map(|s| Fact::decode(&serde_json::from_str(&s)?))
        .transpose()
}
fn current_at(c: &Connection, ns: &Namespace) -> Result<u128> {
    let at: String = c.query_row(
        "SELECT at FROM local_agent_card_source_clock WHERE namespace=?1",
        [ns.as_str()],
        |r| r.get(0),
    )?;
    Ok(at.parse()?)
}
fn txt<'a>(row: &'a Value, key: &str) -> Result<&'a str> {
    row[key]
        .as_str()
        .with_context(|| format!("card source text {key}"))
}
fn local(origin: &str, row: &Value) -> Result<ClaimRecord> {
    let id = row["id"].as_i64().context("local observation signed ID")?;
    let at = row["observed_at_unix_ms"]
        .as_i64()
        .context("local observation signed time")?;
    let actor = match &row["actor"] {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        _ => anyhow::bail!("local observation actor"),
    };
    Ok(ClaimRecord {
        id: local_observation_id(origin, id),
        store_index: row["after_store_index"]
            .as_u64()
            .context("local observation cut")?,
        batch_id: String::new(),
        subject: txt(row, "subject")?.into(),
        kind: txt(row, "kind")?.into(),
        origin: origin.into(),
        actor,
        operation_id: None,
        request_digest: None,
        body: serde_json::from_str(txt(row, "body")?)?,
        predecessors: vec![],
        accepted_at_unix_ms: at.max(0) as u128,
    })
}

impl Kernel {
    fn normalize(&self, tx: &Transaction<'_>, ns: &Namespace, id: &str) -> Result<bool> {
        let next = match shadow::new_fact_in_namespace(tx, ns, id) {
            Ok(f) => f,
            Err(e) if e.downcast_ref::<shadow::Pending>().is_some() => return Ok(false),
            Err(e) => return Err(e),
        };
        let prior = fact(tx, ns, id)?;
        let checkpoint = shadow::raw(
            tx,
            ns,
            "checkpoint_claims",
            &physical_key("checkpoint_claims", json!([id]))?,
        )?;
        let prior_link: Option<String> = tx
            .query_row(
                "SELECT body FROM local_agent_card_source_links WHERE namespace=?1 AND id=?2",
                params![ns.as_str(), id],
                |r| r.get(0),
            )
            .optional()?;
        let next_link = checkpoint.as_ref().map(serde_json::to_string).transpose()?;
        let before = prior.as_ref().map(|f| f.claim.clone()).or_else(|| {
            prior_link
                .as_ref()
                .and_then(|s| serde_json::from_str::<Value>(s).ok())
                .and_then(|v| {
                    v["subject"].as_str().map(|s| ClaimRecord {
                        id: id.into(),
                        subject: s.into(),
                        kind: "checkpoint".into(),
                        origin: String::new(),
                        batch_id: String::new(),
                        store_index: 0,
                        actor: None,
                        operation_id: None,
                        request_digest: None,
                        body: Value::Null,
                        predecessors: vec![],
                        accepted_at_unix_ms: 0,
                    })
                })
        });
        let before_live = prior.as_ref().map(|f| &f.claim);
        for operation in prior
            .iter()
            .chain(next.iter())
            .filter_map(|f| f.claim.operation_id.as_deref())
        {
            queue(tx, ns, "operation", operation)?;
        }
        let now = next.as_ref().map(|f| (&f.claim, &f.key));
        let identity = shadow::parent_identity(tx, ns, id)?;
        agents(
            tx,
            ns,
            agent_authority_ivm::apply_parent_identity(tx, ns, id, identity.as_deref())?,
        )?;
        agents(
            tx,
            ns,
            agent_authority_ivm::apply_claim(tx, ns, before.as_ref(), now)?,
        )?;
        if next.is_none() {
            if let Some(row) = &checkpoint {
                let parents: Vec<String> = serde_json::from_str(txt(row, "predecessors")?)?;
                agents(
                    tx,
                    ns,
                    agent_authority_ivm::apply_tombstone(
                        tx,
                        ns,
                        id,
                        txt(row, "subject")?,
                        &parents,
                    )?,
                )?;
            }
        }
        agents(
            tx,
            ns,
            agent_card_harness::apply_claim(tx, ns, before_live, now)?,
        )?;
        agents(
            tx,
            ns,
            agent_card_signals::apply_claim(tx, ns, before_live, now)?,
        )?;
        agents(
            tx,
            ns,
            agent_card_usage::apply_claim(tx, ns, before_live, now)?,
        )?;
        agents(
            tx,
            ns,
            agent_card_base::apply_claim(tx, ns, before_live, now)?,
        )?;
        agents(
            tx,
            ns,
            agent_card_rollout::apply_claim(tx, ns, before_live, now)?,
        )?;
        agents(
            tx,
            ns,
            agent_card_desired::apply_claim(
                tx,
                ns,
                before_live,
                next.as_ref().filter(|f| !f.repaired).map(|f| &f.claim),
            )?,
        )?;
        let owned = agent_card_owned::apply_claim(
            tx,
            ns,
            before_live,
            next.as_ref().map(|f| agent_card_owned::Captured {
                claim: &f.claim,
                rank: &f.key,
                eligible: !f.repaired,
            }),
        )?;
        agents(tx, ns, owned.affected)?;
        if next
            .as_ref()
            .is_some_and(|f| f.claim.kind == "owned-set.revised")
        {
            let refs=tx.prepare_cached("SELECT key FROM local_agent_owned_dependencies WHERE namespace=?1 AND claim=?2 AND kind='claim' ORDER BY key LIMIT 129")?.query_map(params![ns.as_str(),id],|r|r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
            anyhow::ensure!(
                refs.len() <= 128,
                "owned referenced claim fanout exceeds card budget"
            );
            for reference in refs {
                queue(tx, ns, "owned-reference", &reference)?;
            }
        }
        let aux = agent_card_aux::apply_claim(tx, ns, before_live, now)?;
        anyhow::ensure!(
            aux.unavailable.is_empty(),
            "agent auxiliary source exhausted"
        );
        agents(tx, ns, aux.agents)?;
        agents(
            tx,
            ns,
            agent_card_lifecycle::apply_claim(tx, ns, before_live, now)?,
        )?;
        if next.is_none() {
            agents(tx, ns, agent_card_lifecycle::apply_absence(tx, ns, id)?)?;
        }
        let queue_fact=next.as_ref().filter(|f| matches!(f.claim.kind.as_str(),"step-run.carried"|"work.claimed"|"agent.queue.moved")).map(|f|json!({"id":f.claim.id,"subject":f.claim.subject,"kind":f.claim.kind,"rank":canonical::sortable_key(&f.key),"body":f.claim.body,"eligible":f.claim.kind!="agent.queue.moved" || !f.repaired}));
        ensure_queue(agent_queue::replace_claim(tx, ns, id, queue_fact.as_ref())?)?;
        // Tokens are live claim bodies of any kind. The launch reader deliberately accepts
        // any live body that parses a DesiredSubject/member, matching the legacy lookup.
        agents(
            tx,
            ns,
            agent_card_launch::apply_token(tx, ns, id, next.as_ref().map(|f| &f.claim))?
                .into_iter()
                .map(|k| k.agent),
        )?;
        let old_observation = prior.as_ref().map(|f| agent_card_launch::Observation {
            record: f.claim.clone(),
            order: agent_card_launch::Order::Claim(f.key.clone()),
        });
        let new_observation = next.as_ref().map(|f| agent_card_launch::Observation {
            record: f.claim.clone(),
            order: agent_card_launch::Order::Claim(f.key.clone()),
        });
        agents(
            tx,
            ns,
            agent_card_launch::apply_observation(
                tx,
                ns,
                old_observation.as_ref(),
                new_observation.as_ref(),
            )?
            .into_iter()
            .map(|k| k.agent),
        )?;
        if let Some(old) = before_live {
            agents(tx, ns, [old.subject.clone()])?;
        }
        if let Some(new) = &next {
            agents(tx, ns, [new.claim.subject.clone()])?;
        }
        match &next {
            Some(f) => {
                tx.execute("INSERT INTO local_agent_card_source_facts VALUES(?1,?2,?3,?4) ON CONFLICT(namespace,id) DO UPDATE SET subject=excluded.subject,body=excluded.body",params![ns.as_str(),id,f.claim.subject,serde_json::to_string(&f.encoded())?])?;
            }
            None => {
                tx.execute(
                    "DELETE FROM local_agent_card_source_facts WHERE namespace=?1 AND id=?2",
                    params![ns.as_str(), id],
                )?;
            }
        }
        match next_link {
            Some(body) => {
                tx.execute("INSERT INTO local_agent_card_source_links VALUES(?1,?2,?3) ON CONFLICT(namespace,id) DO UPDATE SET body=excluded.body",params![ns.as_str(),id,body])?;
            }
            None => {
                tx.execute(
                    "DELETE FROM local_agent_card_source_links WHERE namespace=?1 AND id=?2",
                    params![ns.as_str(), id],
                )?;
            }
        }
        shadow::ack(tx, ns, &[id.into()])?;
        Ok(true)
    }
}
fn ensure_queue(coverage: agent_queue::Coverage) -> Result<()> {
    match coverage {
        agent_queue::Coverage::Complete => Ok(()),
        agent_queue::Coverage::Unsupported(reason) => {
            anyhow::bail!("agent queue source unsupported: {reason}")
        }
    }
}

impl Kernel {
    fn physical(&self, tx: &Transaction<'_>, ns: &Namespace, key: &str) -> Result<()> {
        let (table, pk) = key_parts(key)?;
        let new = shadow::raw(tx, ns, &table, key)?;
        let old: Option<String> = tx
            .query_row(
                "SELECT body FROM local_agent_card_source_physical WHERE namespace=?1 AND key=?2",
                params![ns.as_str(), key],
                |r| r.get(0),
            )
            .optional()?;
        let old = old.map(|s| serde_json::from_str::<Value>(&s)).transpose()?;
        match table.as_str() {
            "step_runs" => ensure_queue(agent_queue::replace_step(
                tx,
                ns,
                pk[0].as_str().context("step subject PK")?,
                new.as_ref(),
            )?)?,
            "mission_runs" => ensure_queue(agent_queue::replace_run(
                tx,
                ns,
                pk[0].as_str().context("run ID PK")?,
                new.as_ref(),
            )?)?,
            "operations" => queue(
                tx,
                ns,
                "operation",
                pk[0].as_str().context("operation ID PK")?,
            )?,
            "desired" => agents(
                tx,
                ns,
                [pk[0].as_str().context("desired subject PK")?.into()],
            )?,
            "local_observations" => {
                let old = old.as_ref().map(|r| local(&self.origin, r)).transpose()?;
                let new = new.as_ref().map(|r| local(&self.origin, r)).transpose()?;
                agents(
                    tx,
                    ns,
                    agent_card_signals::apply_local(tx, ns, old.as_ref(), new.as_ref())?,
                )?;
                let physical_id = pk[0].as_i64().context("local PK")?;
                let old_obs = old.map(|record| agent_card_launch::Observation {
                    record,
                    order: agent_card_launch::Order::Local { physical_id },
                });
                let new_obs = new.map(|record| agent_card_launch::Observation {
                    record,
                    order: agent_card_launch::Order::Local { physical_id },
                });
                agents(
                    tx,
                    ns,
                    agent_card_launch::apply_observation(
                        tx,
                        ns,
                        old_obs.as_ref(),
                        new_obs.as_ref(),
                    )?
                    .into_iter()
                    .map(|k| k.agent),
                )?;
            }
            "local_agent_delivery_presence" => agents(
                tx,
                ns,
                [pk[0].as_str().context("delivery recipient PK")?.into()],
            )?,
            "local_agent_card_clock" => {
                let row = new.as_ref().context("captured card clock deleted")?;
                let at: u128 = txt(row, "at_ms")?.parse()?;
                let revision = row["revision"]
                    .as_u64()
                    .context("captured clock revision")?;
                let snapshot_index = row["snapshot_index"]
                    .as_u64()
                    .context("captured activity snapshot position")?;
                anyhow::ensure!(
                    snapshot_index <= i64::MAX as u64,
                    "captured native position exceeds SQL range"
                );
                let prior: Option<(String, u64, u64)> = tx
                    .query_row(
                        "SELECT at,revision,snapshot_index FROM local_agent_card_source_clock WHERE namespace=?1",
                        [ns.as_str()],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                    )
                    .optional()?;
                if let Some((old_at, old_revision, old_index)) = prior {
                    anyhow::ensure!(
                        at >= old_at.parse()? && revision >= old_revision,
                        "captured clock regressed"
                    );
                    if old_index != snapshot_index {
                        let count:usize=tx.query_row("SELECT count(*) FROM (SELECT 1 FROM local_agent_card_source_local_cut WHERE namespace=?1 LIMIT 128)",[ns.as_str()],|r|r.get(0))?;
                        anyhow::ensure!(count < 128, "local activity cut repair backlog exhausted");
                        let lower = old_index.min(snapshot_index);
                        let upper = old_index.max(snapshot_index);
                        tx.execute("INSERT INTO local_agent_card_source_local_cut VALUES(?1,?2,?3,?4,?3,'')",params![ns.as_str(),revision,lower,upper])?;
                    }
                }
                agent_card_signals::set_captured_cut(tx, ns, snapshot_index)?;
                tx.execute("INSERT INTO local_agent_card_source_clock VALUES(?1,?2,?3,0,'','','',?4) ON CONFLICT(namespace) DO UPDATE SET at=excluded.at,revision=excluded.revision,snapshot_index=excluded.snapshot_index",params![ns.as_str(),at.to_string(),revision,snapshot_index])?;
            }
            _ => anyhow::bail!("card unexpected private physical work"),
        }
        if let Some(row) = new {
            tx.execute("INSERT INTO local_agent_card_source_physical VALUES(?1,?2,?3) ON CONFLICT(namespace,key) DO UPDATE SET body=excluded.body",params![ns.as_str(),key,serde_json::to_string(&row)?])?;
        } else {
            tx.execute(
                "DELETE FROM local_agent_card_source_physical WHERE namespace=?1 AND key=?2",
                params![ns.as_str(), key],
            )?;
        }
        ack(tx, ns, "physical", key)
    }

    fn select(&self, tx: &Transaction<'_>, ns: &Namespace, agent: &str) -> Result<bool> {
        let owned = agent_card_owned::read(tx, ns, agent)?;
        if owned.as_ref().is_some_and(|r| !r.blockers.is_empty()) {
            return Ok(false);
        }
        let desired = match owned.as_ref().filter(|r| !r.owners.is_empty()) {
            Some(row) => row.selected.as_ref().and_then(|s| {
                s.desired
                    .clone()
                    .map(|d| (d, s.member.claim.clone(), s.member.revision.clone()))
            }),
            None => agent_card_desired::read(tx, ns, agent)?,
        };
        agent_card_base::replace_desired(
            tx,
            ns,
            agent,
            desired
                .as_ref()
                .map(|(d, t, r)| (d, t.as_str(), r.as_str())),
        )?;
        cards(
            tx,
            ns,
            agent_card_lifecycle::apply_desired(
                tx,
                ns,
                agent,
                desired
                    .as_ref()
                    .map(|(d, t, _)| agent_card_lifecycle::Desired {
                        token: t.clone(),
                        kind: d.kind.clone(),
                    })
                    .as_ref(),
            )?,
        )?;
        ack(tx, ns, "select", agent)?;
        Ok(true)
    }

    fn operation(&self, tx: &Transaction<'_>, ns: &Namespace, id: &str) -> Result<bool> {
        let raw = shadow::raw(
            tx,
            ns,
            "operations",
            &physical_key("operations", json!([id]))?,
        )?;
        let selected = match raw.as_ref().filter(|r| r["state"] == "active") {
            Some(row) => match fact(tx, ns, txt(row, "canonical_claim_id")?)? {
                Some(f) => Some(f.claim),
                None => return Ok(false),
            },
            None => None,
        };
        agent_card_base::replace_operation(
            tx,
            ns,
            id,
            raw.as_ref().map(|r| txt(r, "state")).transpose()?,
        )?;
        agents(
            tx,
            ns,
            agent_card_lifecycle::apply_operation(tx, ns, id, selected.as_ref())?,
        )?;
        ack(tx, ns, "operation", id)?;
        Ok(true)
    }
}

impl Kernel {
    fn reference(&self, tx: &Transaction<'_>, ns: &Namespace, id: &str) -> Result<bool> {
        if agent_card_owned::references(tx, ns, id)? {
            let Some(f) = fact(tx, ns, id)? else {
                return Ok(false);
            };
            let changed = agent_card_owned::apply_claim(
                tx,
                ns,
                None,
                Some(agent_card_owned::Captured {
                    claim: &f.claim,
                    rank: &f.key,
                    eligible: !f.repaired,
                }),
            )?;
            agents(tx, ns, changed.affected)?;
        }
        ack(tx, ns, "owned-reference", id)?;
        Ok(true)
    }

    fn declaration(&self, c: &Connection, ns: &Namespace, agent: &str) -> Result<Option<Value>> {
        let raw = shadow::raw(c, ns, "desired", &physical_key("desired", json!([agent]))?)?;
        let Some(row) = raw else { return Ok(None) };
        let member: Option<crate::model::MemberSpec> = match &row["member"] {
            Value::Null => None,
            Value::String(s) => Some(serde_json::from_str(s)?),
            _ => anyhow::bail!("projected member source type"),
        };
        let Some(member) = member else {
            return Ok(None);
        };
        let body: Value = serde_json::from_str(txt(&row, "body")?)?;
        let checkout = crate::checkout::Checkout::from_desired(&body)
            .map(|c| json!({"repository":c.repository,"base":c.base,"branch":c.branch}));
        Ok(Some(
            json!({"host_id":format!("host/{}",member.host.replace(char::is_whitespace,"-")),"workspace":member.workspace,"checkout":checkout}),
        ))
    }

    fn delivery(
        &self,
        c: &Connection,
        ns: &Namespace,
        agent: &str,
        driver: &str,
        at: u128,
    ) -> Result<Option<(Value, DeliveryCertificate)>> {
        let raw = shadow::raw(
            c,
            ns,
            "local_agent_delivery_presence",
            &physical_key("local_agent_delivery_presence", json!([agent, driver]))?,
        )?;
        let Some(row) = raw else { return Ok(None) };
        if row["state"] != "ready" {
            return Ok(None);
        };
        let cert: DeliveryCertificate = serde_json::from_str(txt(&row, "certificate")?)?;
        anyhow::ensure!(
            cert.recipient == agent
                && cert.driver == driver
                && row["producer_epoch"] == cert.epoch
                && row["producer_revision"].as_u64() == Some(cert.revision)
                && row["evaluation_time_ms"].as_u64() == Some(cert.evaluation_time_ms)
                && row["next_deadline_ms"].as_u64() == Some(cert.next_deadline_ms),
            "captured delivery row certificate mismatch"
        );
        if at < u128::from(cert.evaluation_time_ms) || at >= u128::from(cert.next_deadline_ms) {
            return Ok(None);
        };
        let assessment: Value = serde_json::from_str(txt(&row, "assessment")?)?;
        Ok(Some((assessment, cert)))
    }

    fn materialize(
        &self,
        tx: &Transaction<'_>,
        ns: &Namespace,
        agent: &str,
        at: u128,
        projected: u64,
    ) -> Result<bool> {
        let present:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM local_agent_card_source_facts WHERE namespace=?1 AND subject=?2)",params![ns.as_str(),agent],|r|r.get(0))?;
        let projected_desired =
            shadow::raw(tx, ns, "desired", &physical_key("desired", json!([agent]))?)?.is_some();
        if !present && !projected_desired {
            let changed = agent_card_ivm::write_row(tx, ns, agent, false, None)?;
            self.files(tx, ns, agent, &[])?;
            tx.execute(
                "DELETE FROM local_agent_card_source_cards WHERE namespace=?1 AND agent=?2",
                params![ns.as_str(), agent],
            )?;
            native_binding(tx, ns, agent, None, None)?;
            ack(tx, ns, "card", agent)?;
            agent_queue::acknowledge(tx, ns, &[agent.into()])?;
            return Ok(changed);
        }
        let subject = agent_card_base::subject(tx, ns, agent)?;
        let owned = agent_card_owned::read(tx, ns, agent)?;
        let selection = owned.as_ref().map(|r| r.rollout()).transpose()?.flatten();
        let hold = if selection.as_ref().is_some_and(|s| s.manual) {
            match subject
                .actual
                .as_ref()
                .and_then(|a| a["incarnation_id"].as_str().map(|i| (a, i)))
            {
                Some((actual, inc))
                    if actual["status"] == "running"
                        || matches!(
                            actual["status"].as_str(),
                            Some("stopped" | "exited" | "vanished")
                        ) =>
                {
                    let old = agent_card_launch::read_selected(
                        tx,
                        ns,
                        &agent_card_launch::Key {
                            agent: agent.into(),
                            incarnation: inc.into(),
                        },
                    )?;
                    old.is_none_or(|(_, m)| {
                        selection
                            .as_ref()
                            .unwrap()
                            .desired
                            .member
                            .as_ref()
                            .is_none_or(|new| !new.launch_changes(&m).is_empty())
                    })
                }
                _ => false,
            }
        } else {
            false
        };
        agent_card_rollout::replace_selection(
            tx,
            ns,
            agent,
            selection.as_ref(),
            subject.actual_origin.as_deref(),
            hold,
        )?;
        agent_card_rollout::publish(tx, ns, agent)?;
        agent_card_usage::publish(tx, ns, agent)?;
        let queue = agent_queue::rows(tx, ns, &[agent.into()], at)?
            .remove(agent)
            .unwrap_or_default();
        let ids = queue
            .current_work_ids
            .iter()
            .chain(queue.next_work_id.iter())
            .chain(queue.upcoming_work_ids.iter())
            .cloned()
            .collect::<Vec<_>>();
        let labels = agent_queue::labels(tx, ns, &ids)?;
        let incarnation = subject.harness.as_ref().map(|h| h.incarnation_id.as_str());
        anyhow::ensure!(
            projected == captured_index(tx, ns)?,
            "activity captured cut changed"
        );
        let last_activity_at = agent_card_signals::current_activity(tx, ns, agent, incarnation)?;
        let working_since = incarnation
            .map(|i| agent_card_signals::working_since(tx, ns, agent, i))
            .transpose()?
            .flatten();
        let actual_at = subject
            .actual_claim
            .as_deref()
            .map(|id| agent_card_base::actual_at(tx, ns, id))
            .transpose()?;
        let lifecycle = agent_card_lifecycle::read_lifecycle(tx, ns, agent)?;
        let (todo, session) = agent_card_aux::read_todo_session(tx, ns, agent)?;
        let now: u64 = at.try_into()?;
        let subagents=agent_card_aux::running_subagents_for(tx,ns,agent,now)?.into_iter().map(|s|json!({"id":s.subagent_id,"subagent_type":s.subagent_type,"description":s.description,"driver":s.driver,"session_id":s.session_id,"work_id":s.step_run,"started_at":(s.started_at_unix_ms>0).then(||crate::api::client_timestamp(u128::from(s.started_at_unix_ms))),"lease_expires_at":crate::api::client_timestamp(u128::from(s.lease_expires_at_unix_ms))})).collect();
        let declaration = self.declaration(tx, ns, agent)?;
        let local_host = format!("host/{}", self.origin.replace(char::is_whitespace, "-"));
        let driver = subject
            .harness
            .as_ref()
            .and_then(|h| h.driver.clone())
            .or_else(|| {
                subject.desired.as_ref().and_then(|d| {
                    d["children"]
                        .as_array()?
                        .iter()
                        .find(|c| c["name"] == "harness")?["arguments"]
                        .as_array()?
                        .first()?
                        .as_str()
                        .map(str::to_owned)
                })
            });
        let native_local = driver
            .as_deref()
            .is_some_and(|d| collection_ivm::delivery::DRIVERS.contains(&d))
            && declaration
                .as_ref()
                .is_some_and(|d| d["host_id"] == local_host);
        // Captured assessment is required conservatively for each local native card. This
        // closure is stronger than the formatter's live-state requirement and is explicit.
        let delivery = if native_local {
            let captured = self.delivery(tx, ns, agent, driver.as_deref().unwrap(), at)?;
            native_binding(
                tx,
                ns,
                agent,
                driver.as_deref(),
                captured.as_ref().map(|(_, cert)| cert),
            )?;
            let Some(d) = captured else {
                return Ok(false);
            };
            Some(d)
        } else {
            native_binding(tx, ns, agent, None, None)?;
            None
        };
        let parts = crate::api::agent_card::CardParts {
            subject,
            declaration,
            actual_at,
            last_activity_at,
            working_since,
            queue,
            labels,
            usage: agent_card_usage::summary(tx, ns, agent)?,
            fault: agent_card_aux::read_fault(tx, ns, agent)?,
            handoff: lifecycle.handoff,
            suspension: lifecycle.suspension,
            rollout: agent_card_rollout::read(tx, ns, agent)?,
            todo,
            session,
            subagents,
            delivery: delivery
                .as_ref()
                .map(|(v, _)| (v.clone(), v["state"] == "stale")),
        };
        let formatted = crate::api::agent_card::format(parts, &local_host, at)?;
        let deadline = formatted
            .next_deadline
            .into_iter()
            .chain(agent_card_aux::next_deadline(tx, ns, agent, now)?.map(u128::from))
            .chain(
                delivery
                    .as_ref()
                    .map(|(_, cert)| u128::from(cert.next_deadline_ms)),
            )
            .min();
        let files: Vec<FileEvidence> = delivery
            .as_ref()
            .and_then(|(_, c)| c.followed_file())
            .into_iter()
            .collect();
        let mono = delivery.as_ref().map(|(_, cert)| cert.deadline_ns);
        let certificate = delivery
            .as_ref()
            .map(|(_, c)| serde_json::to_string(c))
            .transpose()?;
        let changed =
            agent_card_ivm::write_row(tx, ns, agent, formatted.current, Some(&formatted.body))?;
        self.files(tx, ns, agent, &files)?;
        tx.execute("INSERT INTO local_agent_card_source_cards VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(namespace,agent) DO UPDATE SET deadline=excluded.deadline,files=excluded.files,mono_deadline=excluded.mono_deadline,certificate=excluded.certificate",params![ns.as_str(),agent,deadline.map(|d|d.to_be_bytes().to_vec()),serde_json::to_string(&files)?,mono,certificate])?;
        ack(tx, ns, "card", agent)?;
        agent_queue::acknowledge(tx, ns, &[agent.into()])?;
        Ok(changed)
    }
}

impl Kernel {
    fn families_closed(&self, c: &Connection, ns: &Namespace, at: u128) -> Result<bool> {
        Ok(self.dependencies_closed(c, ns, at)? && !queue_public_pending(c, ns)?)
    }

    fn dependencies_closed(&self, c: &Connection, ns: &Namespace, at: u128) -> Result<bool> {
        if !shadow::clean(c, ns)?
            || !agent_card_base::clean(c, ns)?
            || !agent_card_usage::clean(c, ns)?
            || !agent_queue::clean(c, ns, at)?
        {
            return Ok(false);
        };
        if agent_authority_ivm::ensure_closed(c, ns).is_err()
            || agent_card_lifecycle::ensure_closed(c, ns).is_err()
            || agent_card_launch::ensure_closed(c, ns).is_err()
        {
            return Ok(false);
        };
        let work:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM local_agent_card_source_work WHERE namespace=?1 AND kind<>'card') OR EXISTS(SELECT 1 FROM local_agent_card_source_local_cut WHERE namespace=?1)",[ns.as_str()],|r|r.get(0))?;
        Ok(!work)
    }

    fn maintain(&self, tx: &Transaction<'_>, ns: &Namespace) -> Result<bool> {
        let at = current_at(tx, ns)?;
        let mut used = 0;
        let mut changed = false;
        // Closed priority stages protect source-cut consistency. Every pending attempt and
        // empty dirty job consumes scheduling work; a captured clock never spins to closure.
        used += shadow::expand_page(tx, ns, WORK)?;
        if used < WORK {
            let after = cursor(tx, ns, "normalize")?;
            let (ids, more) = shadow::dirty_page_after(
                tx,
                ns,
                (!after.is_empty()).then_some(after.as_str()),
                WORK - used,
            )?;
            for id in &ids {
                self.normalize(tx, ns, id)?;
                used += 1;
            }
            set_cursor(
                tx,
                ns,
                "normalize",
                if more {
                    ids.last().map(String::as_str).unwrap_or("")
                } else {
                    ""
                },
            )?;
        }
        if used < WORK {
            for key in page(tx, ns, "physical", WORK - used)? {
                self.physical(tx, ns, &key)?;
                used += 1;
            }
        }
        while used < WORK {
            let job:Option<(u64,u64,u64,u64,String)>=tx.query_row("SELECT revision,lower_index,upper_index,after_index,after_claim FROM local_agent_card_source_local_cut WHERE namespace=?1 ORDER BY revision LIMIT 1",[ns.as_str()],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).optional()?;
            let Some((revision, lower, upper, after, id)) = job else {
                break;
            };
            let (crossed, more) = agent_card_signals::local_cut_page(
                tx,
                ns,
                lower,
                upper,
                Some((after, &id)),
                WORK - used,
            )?;
            used += crossed.len().max(1);
            if more {
                let (at, id, _) = crossed
                    .last()
                    .context("activity cut continuation missing")?;
                tx.execute("UPDATE local_agent_card_source_local_cut SET after_index=?3,after_claim=?4 WHERE namespace=?1 AND revision=?2",params![ns.as_str(),revision,at,id])?;
            } else {
                tx.execute("DELETE FROM local_agent_card_source_local_cut WHERE namespace=?1 AND revision=?2",params![ns.as_str(),revision])?;
            }
            let snapshot_index = captured_index(tx, ns)?;
            for (_, id, _) in &crossed {
                agent_card_signals::repair_local_cut(tx, ns, id, snapshot_index)?;
            }
            cards(tx, ns, crossed.into_iter().map(|(_, _, agent)| agent))?;
        }
        if used < WORK {
            for id in page(tx, ns, "owned-reference", WORK - used)? {
                self.reference(tx, ns, &id)?;
                used += 1;
            }
        }
        if used < WORK {
            for id in page(tx, ns, "operation", WORK - used)? {
                self.operation(tx, ns, &id)?;
                used += 1;
            }
        }
        if used < WORK {
            for agent in page(tx, ns, "select", WORK - used)? {
                self.select(tx, ns, &agent)?;
                used += 1;
            }
        }
        if used < WORK {
            let (n, dirty) = agent_card_base::drain_owners(tx, ns, WORK - used)?;
            used += n;
            cards(tx, ns, dirty)?;
        }
        if used < WORK {
            let (n, dirty) = agent_card_usage::drain(tx, ns, WORK - used)?;
            used += n;
            cards(tx, ns, dirty)?;
        }
        if used < WORK {
            let drained = agent_queue::drain(tx, ns, at, WORK - used)?;
            used += drained.processed;
            ensure_queue(drained.coverage)?;
        }
        if used < WORK && !card_work(tx, ns)? {
            let dirty = agent_queue::dirty_agents(tx, ns, WORK - used)?;
            used += dirty.len();
            cards(tx, ns, dirty)?;
        }
        while used < WORK {
            let cursor:(String,String)=tx.query_row("SELECT authority_agent,authority_id FROM local_agent_card_source_clock WHERE namespace=?1",[ns.as_str()],|r|Ok((r.get(0)?,r.get(1)?)))?;
            let (next, clean) =
                agent_authority_ivm::flush_ancestry(tx, ns, Some((&cursor.0, &cursor.1)), 1)?;
            let (agent, id) = next.clone().unwrap_or_default();
            tx.execute("UPDATE local_agent_card_source_clock SET authority_agent=?2,authority_id=?3 WHERE namespace=?1",params![ns.as_str(),agent,id])?;
            if next.is_none() {
                break;
            }
            used += 1;
            cards(tx, ns, [agent])?;
            if clean {
                break;
            }
        }
        while used < WORK {
            let cursor: String = tx.query_row(
                "SELECT lifecycle_after FROM local_agent_card_source_clock WHERE namespace=?1",
                [ns.as_str()],
                |r| r.get(0),
            )?;
            let repaired = agent_card_lifecycle::flush_page(tx, ns, &cursor, 1)?;
            tx.execute(
                "UPDATE local_agent_card_source_clock SET lifecycle_after=?2 WHERE namespace=?1",
                params![ns.as_str(), repaired.after.as_deref().unwrap_or("")],
            )?;
            cards(tx, ns, repaired.changed)?;
            for need in repaired.missing {
                match need {
                    agent_card_lifecycle::Need::Claim(id) => {
                        if let Some(f) = fact(tx, ns, &id)? {
                            cards(
                                tx,
                                ns,
                                agent_card_lifecycle::apply_claim(
                                    tx,
                                    ns,
                                    None,
                                    Some((&f.claim, &f.key)),
                                )?,
                            )?;
                        } else {
                            cards(tx, ns, agent_card_lifecycle::apply_absence(tx, ns, &id)?)?;
                        }
                    }
                    agent_card_lifecycle::Need::Desired(agent) => queue(tx, ns, "select", &agent)?,
                    agent_card_lifecycle::Need::Operation(id) => queue(tx, ns, "operation", &id)?,
                }
            }
            if repaired.after.is_none() {
                break;
            }
            used += 1;
            if repaired.complete {
                break;
            }
        }
        if used < WORK {
            let encoded = cursor(tx, ns, "launch")?;
            let after = if encoded.is_empty() {
                None
            } else {
                let (agent, incarnation): (String, String) = serde_json::from_str(&encoded)?;
                Some(agent_card_launch::Key { agent, incarnation })
            };
            let repaired = agent_card_launch::flush_page(tx, ns, after.as_ref(), 1)?;
            let next = repaired
                .after
                .as_ref()
                .map(|k| serde_json::to_string(&(k.agent.as_str(), k.incarnation.as_str())))
                .transpose()?
                .unwrap_or_default();
            set_cursor(tx, ns, "launch", &next)?;
            if repaired.after.is_some() {
                used += 1;
            }
            cards(tx, ns, repaired.changed.into_iter().map(|k| k.agent))?;
            for token in repaired.missing {
                let captured = fact(tx, ns, &token)?;
                cards(
                    tx,
                    ns,
                    agent_card_launch::apply_token(
                        tx,
                        ns,
                        &token,
                        captured.as_ref().map(|f| &f.claim),
                    )?
                    .into_iter()
                    .map(|k| k.agent),
                )?;
            }
        }
        if used < WORK && self.dependencies_closed(tx, ns, at)? {
            if !card_work(tx, ns)? {
                let due:Vec<String>=tx.prepare_cached("SELECT agent FROM local_agent_card_source_cards WHERE namespace=?1 AND deadline<=?2 ORDER BY deadline,agent LIMIT ?3")?.query_map(params![ns.as_str(),at.to_be_bytes().as_slice(),WORK-used],|r|r.get(0))?.collect::<rusqlite::Result<_>>()?;
                used += due.len();
                cards(tx, ns, due)?;
            }
            if used < WORK {
                // This reducer describes every input captured in this current-state namespace.
                // Prospective rows are unavailable until independent source-prefix proof and
                // catch-up bind them to the exact current cut. No historical cut is served.
                for agent in page(tx, ns, "card", WORK - used)? {
                    let snapshot_index = captured_index(tx, ns)?;
                    changed |= self.materialize(tx, ns, &agent, at, snapshot_index)?;
                    used += 1;
                }
            }
        }
        anyhow::ensure!(
            used <= WORK,
            "agent card shared maintenance budget exceeded"
        );
        Ok(changed)
    }
}

impl Operator for Kernel {
    fn name(&self) -> &'static str {
        agent_card_ivm::VIEW
    }
    fn fingerprint(&self) -> &'static str {
        agent_card_ivm::FINGERPRINT
    }
    fn source(&self) -> &'static str {
        agent_card_ivm::SOURCE
    }
    fn create_schema(&self, c: &Connection) -> Result<()> {
        shadow::create_schema(c)?;
        agent_source::boundary::create_schema(c)?;
        agent_card_ivm::create_schema(c)?;
        agent_card_harness::create_schema(c)?;
        agent_card_signals::create_schema(c)?;
        agent_card_usage::create_schema(c)?;
        agent_card_base::create_schema(c)?;
        agent_card_desired::create_schema(c)?;
        agent_card_owned::create_schema(c)?;
        agent_card_rollout::create_schema(c)?;
        agent_card_aux::create_schema(c)?;
        agent_authority_ivm::create_schema(c)?;
        agent_card_lifecycle::create_schema(c)?;
        agent_card_launch::create_schema(c)?;
        agent_queue::create_schema(c)?;
        c.execute_batch(SCHEMA)?;
        Ok(())
    }
    fn apply(&self, tx: &Transaction<'_>, ns: &Namespace, rows: &[Mutation]) -> Result<bool> {
        anyhow::ensure!(rows.len() <= WORK, "card physical apply page bound");
        shadow::apply(tx, ns, rows)?;
        tx.execute("INSERT INTO local_agent_card_source_clock VALUES(?1,'0',0,0,'','','',0) ON CONFLICT DO NOTHING",[ns.as_str()])?;
        // Extraction can encounter local rows before its captured clock page. Unknown
        // admission starts at zero, so future anchors are never provisionally eligible.
        tx.execute(
            "INSERT INTO local_agent_card_activity_cut VALUES(?1,0) ON CONFLICT DO NOTHING",
            [ns.as_str()],
        )?;
        let mut captured_clock = false;
        for row in rows {
            let (table, _) = key_parts(&row.key)?;
            match table.as_str() {
                "local_agent_card_clock" => {
                    self.physical(tx, ns, &row.key)?;
                    captured_clock = true;
                }
                "desired"
                | "step_runs"
                | "mission_runs"
                | "operations"
                | "local_observations"
                | "local_agent_delivery_presence" => queue(tx, ns, "physical", &row.key)?,
                _ => {}
            }
        }
        let ready: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM ivm_install_roots WHERE namespace=?1 AND ready=1)",
            [ns.as_str()],
            |r| r.get(0),
        )?;
        if ready && !captured_clock {
            return Ok(false);
        };
        self.maintain(tx, ns)
    }
    fn validate_publication(&self, tx: &Transaction<'_>, ns: &Namespace) -> Result<()> {
        let at = current_at(tx, ns)?;
        anyhow::ensure!(
            self.families_closed(tx, ns, at)?,
            "complete card dependency repair pending"
        );
        anyhow::ensure!(
            !tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM local_agent_card_source_work WHERE namespace=?1)",
                [ns.as_str()],
                |r| r.get::<_, bool>(0)
            )?,
            "complete public card publication pending"
        );
        let stamped:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM local_agent_card_coverage c JOIN ivm_install_sources s ON s.name=?2 AND s.epoch=c.source_epoch AND s.revision=c.source_revision AND s.available=1 WHERE c.namespace=?1 AND c.incomplete=0 AND c.pending=0 AND c.evaluation_time=?3 AND c.projected=?4)",params![ns.as_str(),agent_card_ivm::SOURCE,at.to_string(),captured_index(tx,ns)?],|r|r.get(0))?;
        anyhow::ensure!(
            stamped,
            "certified source cut/producer coverage not stamped"
        );
        anyhow::ensure!(
            next_deadline(tx, ns)?.is_none_or(|d| at < d),
            "card publication deadline expired"
        );
        Ok(())
    }
    fn reclaim(&self, tx: &Transaction<'_>, ns: &Namespace, rows: usize) -> Result<bool> {
        anyhow::ensure!(
            (1..=WORK).contains(&rows),
            "card namespace reclamation budget"
        );
        tx.execute(
            "INSERT INTO local_agent_card_source_reclaim VALUES(?1,0) ON CONFLICT DO NOTHING",
            [ns.as_str()],
        )?;
        let phase: u64 = tx.query_row(
            "SELECT phase FROM local_agent_card_source_reclaim WHERE namespace=?1",
            [ns.as_str()],
            |r| r.get(0),
        )?;
        // One component only per call: a boolean completion can include deleted rows.
        // Advancing the durable phase never gives the next component a second full budget.
        let done = match phase {
            0 => {
                let (_, done) = shadow::reclaim(tx, ns, rows)?;
                done
            }
            1 => agent_card_owned::reclaim(tx, ns, rows)?,
            2 => agent_queue::reclaim(tx, ns, rows)?,
            3 => agent_authority_ivm::reclaim_namespace(tx, ns, rows)?,
            4 => agent_card_lifecycle::reclaim_namespace(tx, ns, rows)?,
            5 => agent_card_launch::reclaim_namespace(tx, ns, rows)?,
            6 => {
                let (_, done) = agent_source::boundary::reclaim(tx, ns, rows)?;
                done
            }
            7 => reclaim_remaining(tx, ns, rows)?,
            8 => {
                tx.execute(
                    "DELETE FROM local_agent_card_source_reclaim WHERE namespace=?1",
                    [ns.as_str()],
                )?;
                return Ok(true);
            }
            _ => anyhow::bail!("agent card reclaim continuation invalid"),
        };
        if done {
            tx.execute(
                "UPDATE local_agent_card_source_reclaim SET phase=phase+1 WHERE namespace=?1",
                [ns.as_str()],
            )?;
        }
        Ok(false)
    }
}

impl Kernel {
    fn files(
        &self,
        tx: &Transaction<'_>,
        ns: &Namespace,
        agent: &str,
        new: &[FileEvidence],
    ) -> Result<()> {
        let old: Option<String> = tx
            .query_row(
                "SELECT files FROM local_agent_card_source_cards WHERE namespace=?1 AND agent=?2",
                params![ns.as_str(), agent],
                |r| r.get(0),
            )
            .optional()?;
        let old: Vec<FileEvidence> = old
            .map(|s| serde_json::from_str(&s))
            .transpose()?
            .unwrap_or_default();
        anyhow::ensure!(
            old.len() <= 1 && new.len() <= 1,
            "card followed-file multiplicity"
        );
        if old == new {
            return Ok(());
        };
        for file in old {
            tx.execute("DELETE FROM local_agent_card_source_files WHERE namespace=?1 AND path=?2 AND identity=?3 AND count=1",params![ns.as_str(),file.path,file.identity])?;
            tx.execute("UPDATE local_agent_card_source_files SET count=count-1 WHERE namespace=?1 AND path=?2 AND identity=?3",params![ns.as_str(),file.path,file.identity])?;
        }
        for file in new {
            tx.execute("INSERT INTO local_agent_card_source_files VALUES(?1,?2,?3,1) ON CONFLICT(namespace,path,identity) DO UPDATE SET count=count+1",params![ns.as_str(),file.path,file.identity])?;
        }
        Ok(())
    }
}

pub(crate) struct Footprint {
    pub files: Vec<FileEvidence>,
    pub earliest_monotonic_deadline: Option<u64>,
}
/// Complete indexed file/deadline evidence only after every namespace family and card closes.
/// This is not a producer or graph/source certificate, nor selected-window evidence.
pub(crate) fn footprint(c: &Connection, ns: &Namespace, origin: &str) -> Result<Footprint> {
    let kernel = Kernel::new(origin);
    anyhow::ensure!(
        kernel.families_closed(c, ns, current_at(c, ns)?)?,
        "card namespace dependencies pending"
    );
    anyhow::ensure!(
        !c.query_row(
            "SELECT EXISTS(SELECT 1 FROM local_agent_card_source_work WHERE namespace=?1)",
            [ns.as_str()],
            |r| r.get::<_, bool>(0)
        )?,
        "card namespace materialization pending"
    );
    let files:Vec<FileEvidence>=c.prepare_cached("SELECT path,identity FROM local_agent_card_source_files WHERE namespace=?1 ORDER BY path,identity LIMIT 65")?.query_map([ns.as_str()],|r|Ok(FileEvidence {path:r.get(0)?,identity:r.get(1)?}))?.collect::<rusqlite::Result<_>>()?;
    anyhow::ensure!(
        files.len() <= 64 && files.windows(2).all(|w| w[0].path != w[1].path),
        "namespace file footprint exhausted or conflicting"
    );
    let deadline=c.query_row("SELECT mono_deadline FROM local_agent_card_source_cards WHERE namespace=?1 AND mono_deadline IS NOT NULL ORDER BY mono_deadline,agent LIMIT 1",[ns.as_str()],|r|r.get(0)).optional()?;
    Ok(Footprint {
        files,
        earliest_monotonic_deadline: deadline,
    })
}

pub(crate) fn next_deadline(c: &Connection, ns: &Namespace) -> Result<Option<u128>> {
    let deadline:Option<Vec<u8>>=c.query_row("SELECT deadline FROM local_agent_card_source_cards WHERE namespace=?1 AND deadline IS NOT NULL ORDER BY deadline,agent LIMIT 1",[ns.as_str()],|r|r.get(0)).optional()?;
    let deadline = deadline
        .map(|d| {
            Ok::<_, anyhow::Error>(u128::from_be_bytes(
                d.try_into()
                    .map_err(|_| anyhow::anyhow!("card deadline width"))?,
            ))
        })
        .transpose()?;
    Ok(deadline
        .into_iter()
        .chain(agent_queue::next_deadline(c, ns)?)
        .min())
}

/// The shared source owner calls this after its complete scan/journal/native/healthy-frontier
/// proof and producer boundary commit. It writes private coverage, never Installer readiness.
pub(crate) fn certify_coverage(
    tx: &Transaction<'_>,
    ns: &Namespace,
    origin: &str,
    position: &SourcePosition,
    cut: &smallclaims::ivm::SourceCut,
    captured_at: u128,
) -> Result<()> {
    anyhow::ensure!(
        position.source == agent_card_ivm::SOURCE
            && position.fingerprint == agent_source::capture_fingerprint_for(origin)?,
        "agent card certified source binding mismatch"
    );
    anyhow::ensure!(
        cut.admitted == cut.projected && cut.projected == captured_index(tx, ns)?,
        "agent card healthy prefix not complete"
    );
    let at = current_at(tx, ns)?;
    anyhow::ensure!(
        captured_at == at,
        "card rows require exact captured source clock"
    );
    footprint(tx, ns, origin)?;
    let deadline = next_deadline(tx, ns)?;
    anyhow::ensure!(
        deadline.is_none_or(|d| captured_at < d),
        "agent card presentation deadline expired"
    );
    let available:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM ivm_install_sources WHERE name=?1 AND fingerprint=?2 AND epoch=?3 AND revision=?4 AND available=1)",params![position.source,position.fingerprint,position.epoch,position.revision],|r|r.get(0))?;
    anyhow::ensure!(available, "agent card source position changed or fenced");
    tx.execute("INSERT INTO local_agent_card_coverage VALUES(?1,0,0,?2,?3,?4,?5,?6,?7,?8) ON CONFLICT(namespace) DO UPDATE SET incomplete=0,pending=0,source_epoch=excluded.source_epoch,source_revision=excluded.source_revision,graph_epoch=excluded.graph_epoch,projected=excluded.projected,local_generation=excluded.local_generation,evaluation_time=excluded.evaluation_time,next_deadline=excluded.next_deadline",params![ns.as_str(),position.epoch,position.revision,cut.epoch,cut.projected,cut.local_generation,captured_at.to_string(),deadline.map(|d|d.to_string())])?;
    Ok(())
}

fn reclaim_remaining(tx: &Transaction<'_>, ns: &Namespace, limit: usize) -> Result<bool> {
    const PAGES: &[&str] = &[
        "DELETE FROM local_agent_card_rows WHERE namespace=?1 AND rowid IN (SELECT rowid FROM local_agent_card_rows WHERE namespace=?1 LIMIT ?2)",
        "DELETE FROM local_agent_card_coverage WHERE namespace=?1 AND rowid IN (SELECT rowid FROM local_agent_card_coverage WHERE namespace=?1 LIMIT ?2)",
        "DELETE FROM local_agent_card_harness_rows WHERE namespace=?1 AND rowid IN (SELECT rowid FROM local_agent_card_harness_rows WHERE namespace=?1 LIMIT ?2)",
        "DELETE FROM local_agent_card_harness_nodes WHERE namespace=?1 AND rowid IN (SELECT rowid FROM local_agent_card_harness_nodes WHERE namespace=?1 LIMIT ?2)",
        "DELETE FROM local_agent_card_harness_roots WHERE namespace=?1 AND rowid IN (SELECT rowid FROM local_agent_card_harness_roots WHERE namespace=?1 LIMIT ?2)",
        "DELETE FROM local_agent_card_harness_sequence WHERE namespace=?1 AND rowid IN (SELECT rowid FROM local_agent_card_harness_sequence WHERE namespace=?1 LIMIT ?2)",
        "DELETE FROM local_agent_card_activity_cut WHERE namespace=?1 AND rowid IN (SELECT rowid FROM local_agent_card_activity_cut WHERE namespace=?1 LIMIT ?2)",
        "DELETE FROM local_agent_card_activity_inputs WHERE namespace=?1 AND rowid IN (SELECT rowid FROM local_agent_card_activity_inputs WHERE namespace=?1 LIMIT ?2)",
        "DELETE FROM local_agent_card_working_inputs WHERE namespace=?1 AND rowid IN (SELECT rowid FROM local_agent_card_working_inputs WHERE namespace=?1 LIMIT ?2)",
        "DELETE FROM local_agent_card_usage_inputs WHERE namespace=?1 AND rowid IN (SELECT rowid FROM local_agent_card_usage_inputs WHERE namespace=?1 LIMIT ?2)",
        "DELETE FROM local_agent_card_usage_groups WHERE namespace=?1 AND rowid IN (SELECT rowid FROM local_agent_card_usage_groups WHERE namespace=?1 LIMIT ?2)",
        "DELETE FROM local_agent_card_usage_rollups WHERE namespace=?1 AND rowid IN (SELECT rowid FROM local_agent_card_usage_rollups WHERE namespace=?1 LIMIT ?2)",
        "DELETE FROM local_agent_card_usage_dirty WHERE namespace=?1 AND rowid IN (SELECT rowid FROM local_agent_card_usage_dirty WHERE namespace=?1 LIMIT ?2)",
        "DELETE FROM local_agent_card_usage_rows WHERE namespace=?1 AND rowid IN (SELECT rowid FROM local_agent_card_usage_rows WHERE namespace=?1 LIMIT ?2)",
        "DELETE FROM local_agent_card_base_claims WHERE namespace=?1 AND rowid IN (SELECT rowid FROM local_agent_card_base_claims WHERE namespace=?1 LIMIT ?2)",
        "DELETE FROM local_agent_card_owner_fields WHERE namespace=?1 AND rowid IN (SELECT rowid FROM local_agent_card_owner_fields WHERE namespace=?1 LIMIT ?2)",
        "DELETE FROM local_agent_card_base_declarations WHERE namespace=?1 AND rowid IN (SELECT rowid FROM local_agent_card_base_declarations WHERE namespace=?1 LIMIT ?2)",
        "DELETE FROM local_agent_card_base_desired WHERE namespace=?1 AND rowid IN (SELECT rowid FROM local_agent_card_base_desired WHERE namespace=?1 LIMIT ?2)",
        "DELETE FROM local_agent_card_owner_dependents WHERE namespace=?1 AND rowid IN (SELECT rowid FROM local_agent_card_owner_dependents WHERE namespace=?1 LIMIT ?2)",
        "DELETE FROM local_agent_card_owner_dirty WHERE namespace=?1 AND rowid IN (SELECT rowid FROM local_agent_card_owner_dirty WHERE namespace=?1 LIMIT ?2)",
        "DELETE FROM local_agent_card_base_conflicts WHERE namespace=?1 AND rowid IN (SELECT rowid FROM local_agent_card_base_conflicts WHERE namespace=?1 LIMIT ?2)",
        "DELETE FROM local_agent_card_base_missing_operations WHERE namespace=?1 AND rowid IN (SELECT rowid FROM local_agent_card_base_missing_operations WHERE namespace=?1 LIMIT ?2)",
        "DELETE FROM local_agent_card_operation_states WHERE namespace=?1 AND rowid IN (SELECT rowid FROM local_agent_card_operation_states WHERE namespace=?1 LIMIT ?2)",
        "DELETE FROM local_agent_card_desired_inputs WHERE namespace=?1 AND rowid IN (SELECT rowid FROM local_agent_card_desired_inputs WHERE namespace=?1 LIMIT ?2)",
        "DELETE FROM local_agent_card_desired_refs WHERE namespace=?1 AND rowid IN (SELECT rowid FROM local_agent_card_desired_refs WHERE namespace=?1 LIMIT ?2)",
        "DELETE FROM local_agent_card_rollout_inputs WHERE namespace=?1 AND rowid IN (SELECT rowid FROM local_agent_card_rollout_inputs WHERE namespace=?1 LIMIT ?2)",
        "DELETE FROM local_agent_card_rollout_fields WHERE namespace=?1 AND rowid IN (SELECT rowid FROM local_agent_card_rollout_fields WHERE namespace=?1 LIMIT ?2)",
        "DELETE FROM local_agent_card_rollout_selection WHERE namespace=?1 AND rowid IN (SELECT rowid FROM local_agent_card_rollout_selection WHERE namespace=?1 LIMIT ?2)",
        "DELETE FROM local_agent_card_rollout_rows WHERE namespace=?1 AND rowid IN (SELECT rowid FROM local_agent_card_rollout_rows WHERE namespace=?1 LIMIT ?2)",
        "DELETE FROM local_agent_card_aux_nodes WHERE namespace=?1 AND rowid IN (SELECT rowid FROM local_agent_card_aux_nodes WHERE namespace=?1 LIMIT ?2)",
        "DELETE FROM local_agent_card_aux_agents WHERE namespace=?1 AND rowid IN (SELECT rowid FROM local_agent_card_aux_agents WHERE namespace=?1 LIMIT ?2)",
        "DELETE FROM local_agent_card_aux_groups WHERE namespace=?1 AND rowid IN (SELECT rowid FROM local_agent_card_aux_groups WHERE namespace=?1 LIMIT ?2)",
        "DELETE FROM local_agent_card_aux_subagents WHERE namespace=?1 AND rowid IN (SELECT rowid FROM local_agent_card_aux_subagents WHERE namespace=?1 LIMIT ?2)",
        "DELETE FROM local_agent_card_source_facts WHERE namespace=?1 AND rowid IN (SELECT rowid FROM local_agent_card_source_facts WHERE namespace=?1 LIMIT ?2)",
        "DELETE FROM local_agent_card_source_links WHERE namespace=?1 AND rowid IN (SELECT rowid FROM local_agent_card_source_links WHERE namespace=?1 LIMIT ?2)",
        "DELETE FROM local_agent_card_source_physical WHERE namespace=?1 AND rowid IN (SELECT rowid FROM local_agent_card_source_physical WHERE namespace=?1 LIMIT ?2)",
        "DELETE FROM local_agent_card_source_cursor WHERE namespace=?1 AND rowid IN (SELECT rowid FROM local_agent_card_source_cursor WHERE namespace=?1 LIMIT ?2)",
        "DELETE FROM local_agent_card_source_work WHERE namespace=?1 AND rowid IN (SELECT rowid FROM local_agent_card_source_work WHERE namespace=?1 LIMIT ?2)",
        "DELETE FROM local_agent_card_source_cards WHERE namespace=?1 AND rowid IN (SELECT rowid FROM local_agent_card_source_cards WHERE namespace=?1 LIMIT ?2)",
        "DELETE FROM local_agent_card_source_native WHERE namespace=?1 AND rowid IN (SELECT rowid FROM local_agent_card_source_native WHERE namespace=?1 LIMIT ?2)",
        "DELETE FROM local_agent_card_source_files WHERE namespace=?1 AND rowid IN (SELECT rowid FROM local_agent_card_source_files WHERE namespace=?1 LIMIT ?2)",
        "DELETE FROM local_agent_card_source_local_cut WHERE namespace=?1 AND rowid IN (SELECT rowid FROM local_agent_card_source_local_cut WHERE namespace=?1 LIMIT ?2)",
        "DELETE FROM local_agent_card_source_clock WHERE namespace=?1 AND rowid IN (SELECT rowid FROM local_agent_card_source_clock WHERE namespace=?1 LIMIT ?2)",
    ];
    for sql in PAGES {
        if tx.execute(sql, params![ns.as_str(), limit])? != 0 {
            return Ok(false);
        }
    }
    Ok(true)
}

fn native_binding(
    tx: &Transaction<'_>,
    ns: &Namespace,
    agent: &str,
    driver: Option<&str>,
    cert: Option<&DeliveryCertificate>,
) -> Result<()> {
    if let Some(driver) = driver {
        anyhow::ensure!(
            collection_ivm::delivery::DRIVERS.contains(&driver),
            "unknown native source driver"
        );
        if let Some(cert) = cert {
            anyhow::ensure!(
                cert.recipient == agent && cert.driver == driver,
                "native source classifier certificate binding"
            );
        }
        tx.execute("INSERT INTO local_agent_card_source_native VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(namespace,agent) DO UPDATE SET driver=excluded.driver,epoch=excluded.epoch,deadline=excluded.deadline,needed=excluded.needed",params![ns.as_str(),agent,driver,cert.map(|c|c.epoch.as_str()).unwrap_or(""),cert.map(|c|u128::from(c.next_deadline_ms).to_be_bytes().to_vec()),cert.is_none()])?;
    } else {
        tx.execute(
            "DELETE FROM local_agent_card_source_native WHERE namespace=?1 AND agent=?2",
            params![ns.as_str(), agent],
        )?;
    }
    Ok(())
}

/// Requests come from the repaired namespace classifier before public-row creation. Each
/// indexed range consumes a shared candidate budget, including duplicate candidates. The
/// source owner captures all five drivers outside the writer and journals their full rows.
/// An empty request page is neither dependency closure nor a live producer certificate:
/// same-epoch live fences/revisions must also be checked by the source owner.
pub(crate) fn producer_requests(
    c: &Connection,
    ns: &Namespace,
    current_producer_epoch: &str,
    at: u64,
    limit: usize,
) -> Result<Vec<(String, String)>> {
    anyhow::ensure!((1..=WORK).contains(&limit), "native source request bound");
    anyhow::ensure!(
        !current_producer_epoch.is_empty() && current_producer_epoch.len() <= 256,
        "native source request epoch"
    );
    let queries = [
        "SELECT agent,driver FROM local_agent_card_source_native INDEXED BY local_agent_card_source_native_needed WHERE namespace=?1 AND needed=1 ORDER BY agent,driver LIMIT ?4",
        "SELECT agent,driver FROM local_agent_card_source_native INDEXED BY local_agent_card_source_native_epoch WHERE namespace=?1 AND needed=0 AND epoch<?2 ORDER BY epoch,agent,driver LIMIT ?4",
        "SELECT agent,driver FROM local_agent_card_source_native INDEXED BY local_agent_card_source_native_epoch WHERE namespace=?1 AND needed=0 AND epoch>?2 ORDER BY epoch,agent,driver LIMIT ?4",
        "SELECT agent,driver FROM local_agent_card_source_native INDEXED BY local_agent_card_source_native_deadline WHERE namespace=?1 AND needed=0 AND deadline<=?3 ORDER BY deadline,agent,driver LIMIT ?4",
    ];
    let mut used = 0;
    let mut requested = BTreeSet::new();
    for sql in queries {
        if used == limit {
            break;
        }
        let page = c
            .prepare_cached(sql)?
            .query_map(
                params![
                    ns.as_str(),
                    current_producer_epoch,
                    u128::from(at).to_be_bytes().as_slice(),
                    limit - used
                ],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        used += page.len();
        requested.extend(page);
    }
    Ok(requested.into_iter().collect())
}

/// Private row certificates for an already authorized bounded public selection. The caller
/// validates the full namespace boundary even when this selection contains no native cards.
pub(crate) fn selected_certificates(
    c: &Connection,
    ns: &Namespace,
    agents: &[String],
) -> Result<Vec<DeliveryCertificate>> {
    anyhow::ensure!(
        agents.len() <= agent_card_ivm::WINDOW_LIMIT,
        "card certificate selection bound"
    );
    let rows:Vec<String>=c.prepare_cached("SELECT certificate FROM local_agent_card_source_cards WHERE namespace=?1 AND agent IN (SELECT value FROM json_each(?2)) AND certificate IS NOT NULL")?.query_map(params![ns.as_str(),serde_json::to_string(agents)?],|r|r.get(0))?.collect::<rusqlite::Result<_>>()?;
    rows.into_iter()
        .map(|s| serde_json::from_str(&s).map_err(Into::into))
        .collect()
}

#[cfg(test)]
#[path = "agent_card_source/tests.rs"]
pub(crate) mod tests;

#[cfg(test)]
#[path = "agent_card_source/queue_controls.rs"]
mod queue_controls;
