//! Captured launch receipts and live-token members for the full agent card.
//! Log order includes local physical observations; it is not canonical claim order.
//! This dependency has no source/global lookup, clock, registry or Publisher. The caller
//! proves complete claims/local/token extraction at the same namespace cut and authority.
use super::*;
use crate::model::MemberSpec;
use smallclaims::ivm::install::Namespace;

pub(crate) const FINGERPRINT: &str = "agent-launch.v1;namespace.v1;claim-log-order.v1;stable-physical-ties.v1;live-token-member.v1;bounds128.v1";
const BOUND: usize = 128;
fn sql(ns: &Namespace, q: &str) -> String {
    q.replace("@NS@", &format!("'{}'", ns.as_str().replace('\'', "''")))
}
pub(crate) fn create_schema(c: &Connection) -> Result<()> {
    c.execute_batch(
        r#"
CREATE TABLE IF NOT EXISTS local_agent_launch_tokens(
 namespace TEXT NOT NULL, id TEXT NOT NULL, member TEXT, PRIMARY KEY(namespace,id)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS local_agent_launch_candidates(
 namespace TEXT NOT NULL, source INTEGER NOT NULL, id TEXT NOT NULL,
 agent TEXT NOT NULL, incarnation TEXT NOT NULL, token TEXT NOT NULL, ordering BLOB NOT NULL,
 known INTEGER NOT NULL, member TEXT, PRIMARY KEY(namespace,source,id)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS agent_launch_reverse
 ON local_agent_launch_candidates(namespace,token,source,id);
CREATE INDEX IF NOT EXISTS agent_launch_valid
 ON local_agent_launch_candidates(namespace,agent,incarnation,ordering DESC,source,id)
 WHERE member IS NOT NULL;
CREATE INDEX IF NOT EXISTS agent_launch_unknown
 ON local_agent_launch_candidates(namespace,agent,incarnation,token)
 WHERE known=0;
CREATE TABLE IF NOT EXISTS local_agent_launch_dirty(
 namespace TEXT NOT NULL, agent TEXT NOT NULL, incarnation TEXT NOT NULL,
 PRIMARY KEY(namespace,agent,incarnation)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS local_agent_launch_rows(
 namespace TEXT NOT NULL, agent TEXT NOT NULL, incarnation TEXT NOT NULL, body TEXT NOT NULL,
 PRIMARY KEY(namespace,agent,incarnation)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS local_agent_launch_fences(
 namespace TEXT PRIMARY KEY, reason TEXT NOT NULL
) WITHOUT ROWID;
"#,
    )?;
    Ok(())
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Key {
    pub agent: String,
    pub incarnation: String,
}
#[derive(Clone, Debug)]
pub(crate) enum Order {
    Claim(canonical::ClaimKey),
    Local { physical_id: i64 },
}
#[derive(Clone, Debug)]
pub(crate) struct Observation {
    pub record: ClaimRecord,
    pub order: Order,
}
impl Observation {
    fn identity(&self) -> Result<(i64, String)> {
        match &self.order {
            Order::Claim(key) => {
                anyhow::ensure!(
                    key.5 == self.record.id
                        && key.0 == self.record.accepted_at_unix_ms
                        && key.3 == self.record.batch_id,
                    "launch canonical tie key identity mismatch"
                );
                Ok((0, self.record.id.clone()))
            }
            Order::Local { physical_id } => {
                anyhow::ensure!(
                    self.record.id == local_observation_id(&self.record.origin, *physical_id),
                    "launch local physical identity mismatch"
                );
                Ok((1, physical_id.to_string()))
            }
        }
    }
    fn ordering(&self) -> Vec<u8> {
        let (index, local) = claim_log_order(&self.record);
        let mut rank = Vec::from(index.to_be_bytes());
        rank.extend_from_slice(&local.to_be_bytes());
        match &self.order {
            // Stable sort in observations_for begins with canonical claims, then local
            // rows in signed physical ID order. Only equal log-order keys reach this tie.
            Order::Claim(key) => {
                rank.push(0);
                rank.extend(canonical::sortable_key(key));
            }
            Order::Local { physical_id } => {
                rank.push(1);
                rank.extend_from_slice(&((*physical_id as u64) ^ (1 << 63)).to_be_bytes());
            }
        }
        rank
    }
}
fn queue(tx: &Transaction<'_>, ns: &Namespace, key: &Key) -> Result<()> {
    tx.execute(
        &sql(
            ns,
            "INSERT INTO local_agent_launch_dirty VALUES(@NS@,?1,?2) ON CONFLICT DO NOTHING",
        ),
        params![key.agent, key.incarnation],
    )?;
    Ok(())
}
fn fence(tx: &Transaction<'_>, ns: &Namespace, reason: &str) -> Result<()> {
    tx.execute(&sql(ns,"INSERT INTO local_agent_launch_fences VALUES(@NS@,?1) ON CONFLICT(namespace) DO UPDATE SET reason=excluded.reason"),[reason])?;
    Ok(())
}
fn remove(
    tx: &Transaction<'_>,
    ns: &Namespace,
    source: i64,
    id: &str,
    changed: &mut BTreeSet<Key>,
) -> Result<()> {
    let key:Option<Key>=tx.query_row(&sql(ns,"SELECT agent,incarnation FROM local_agent_launch_candidates WHERE namespace=@NS@ AND source=?1 AND id=?2"),params![source,id],|r|Ok(Key{agent:r.get(0)?,incarnation:r.get(1)?})).optional()?;
    if let Some(key) = key {
        tx.execute(&sql(ns,"DELETE FROM local_agent_launch_candidates WHERE namespace=@NS@ AND source=?1 AND id=?2"),params![source,id])?;
        queue(tx, ns, &key)?;
        changed.insert(key);
    }
    Ok(())
}

/// Complete old/staged/new physical facts, including metadata/order corrections and local
/// deletion. The physical source kind/ID must be retained independently of logical tokens.
pub(crate) fn apply_observation(
    tx: &Transaction<'_>,
    ns: &Namespace,
    old: Option<&Observation>,
    new: Option<&Observation>,
) -> Result<BTreeSet<Key>> {
    let mut changed = BTreeSet::new();
    if let Some(old) = old {
        let (source, id) = old.identity()?;
        remove(tx, ns, source, &id, &mut changed)?;
    }
    if let Some(new) = new {
        let (source, id) = new.identity()?;
        remove(tx, ns, source, &id, &mut changed)?;
        let c = &new.record;
        let fields = c.body.get("fields").unwrap_or(&c.body);
        if c.kind == "runtime.action.succeeded"
            && fields["action"] == "start"
            && let (Some(inc), Some(token)) = (
                fields["incarnation_id"].as_str(),
                fields["desired_token"].as_str(),
            )
        {
            let key = Key {
                agent: c.subject.clone(),
                incarnation: inc.into(),
            };
            let member:Option<Option<String>>=tx.query_row(&sql(ns,"SELECT member FROM local_agent_launch_tokens WHERE namespace=@NS@ AND id=?1"),[token],|r|r.get(0)).optional()?;
            tx.execute(&sql(ns,"INSERT INTO local_agent_launch_candidates VALUES(@NS@,?1,?2,?3,?4,?5,?6,?7,?8)"),params![source,id,key.agent,key.incarnation,token,new.ordering(),member.is_some(),member.flatten()])?;
            queue(tx, ns, &key)?;
            changed.insert(key);
        }
    }
    Ok(changed)
}

/// The raw ID resolves to a LIVE claim at the captured cut, or explicit None for absence
/// (including checkpoint-only identity). Any live body parsing DesiredSubject/member is
/// eligible, just as launched_member; do not impose a token subject/kind restriction here.
pub(crate) fn apply_token(
    tx: &Transaction<'_>,
    ns: &Namespace,
    id: &str,
    live: Option<&ClaimRecord>,
) -> Result<BTreeSet<Key>> {
    if let Some(c) = live {
        anyhow::ensure!(c.id == id, "launch token live identity mismatch");
    }
    let member = live
        .and_then(|c| serde_json::from_value::<DesiredSubject>(c.body.clone()).ok())
        .and_then(|d| d.member)
        .map(|m| serde_json::to_string(&m))
        .transpose()?;
    let prior: Option<Option<String>> = tx
        .query_row(
            &sql(
                ns,
                "SELECT member FROM local_agent_launch_tokens WHERE namespace=@NS@ AND id=?1",
            ),
            [id],
            |r| r.get(0),
        )
        .optional()?;
    if prior
        .as_ref()
        .is_some_and(|old| old.as_deref() == member.as_deref())
    {
        return Ok(BTreeSet::new());
    }
    tx.execute(&sql(ns,"INSERT INTO local_agent_launch_tokens VALUES(@NS@,?1,?2) ON CONFLICT(namespace,id) DO UPDATE SET member=excluded.member"),params![id,member])?;
    let candidates=tx.prepare_cached(&sql(ns,"SELECT source,id,agent,incarnation FROM local_agent_launch_candidates WHERE namespace=@NS@ AND token=?1 ORDER BY source,id LIMIT ?2"))?
      .query_map(params![id,BOUND+1],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?,Key{agent:r.get(2)?,incarnation:r.get(3)?})))?.collect::<rusqlite::Result<Vec<_>>>()?;
    if candidates.len() > BOUND {
        fence(tx, ns, "launch token reverse fanout exceeds bound")?;
    }
    let mut changed = BTreeSet::new();
    for (source, candidate, key) in candidates.into_iter().take(BOUND) {
        tx.execute(&sql(ns,"UPDATE local_agent_launch_candidates SET known=1,member=?3 WHERE namespace=@NS@ AND source=?1 AND id=?2"),params![source,candidate,member])?;
        queue(tx, ns, &key)?;
        changed.insert(key);
    }
    Ok(changed)
}

pub(crate) struct Page {
    pub after: Option<Key>,
    pub changed: BTreeSet<Key>,
    pub missing: BTreeSet<String>,
    pub complete: bool,
}
/// Repairs at most limit pairs, with at most 128 unresolved candidate rows plus one
/// overflow probe across the whole page. Pending pairs advance the cursor; they remain
/// dirty until their captured token inputs arrive. The owner shares its overall budget.
pub(crate) fn flush_page(
    tx: &Transaction<'_>,
    ns: &Namespace,
    after: Option<&Key>,
    limit: usize,
) -> Result<Page> {
    anyhow::ensure!(
        (1..=BOUND).contains(&limit),
        "launch repair page exceeds bound"
    );
    let (agent, inc) = after
        .map(|k| (k.agent.as_str(), k.incarnation.as_str()))
        .unwrap_or(("", ""));
    let keys=tx.prepare_cached(&sql(ns,"SELECT agent,incarnation FROM local_agent_launch_dirty WHERE namespace=@NS@ AND (agent,incarnation)>(?1,?2) ORDER BY agent,incarnation LIMIT ?3"))?
      .query_map(params![agent,inc,limit],|r|Ok(Key{agent:r.get(0)?,incarnation:r.get(1)?}))?.collect::<rusqlite::Result<Vec<_>>>()?;
    let mut page = Page {
        after: None,
        changed: BTreeSet::new(),
        missing: BTreeSet::new(),
        complete: false,
    };
    let mut missing_budget = BOUND;
    for key in keys {
        page.after = Some(key.clone());
        let missing=tx.prepare_cached(&sql(ns,"SELECT token FROM local_agent_launch_candidates WHERE namespace=@NS@ AND agent=?1 AND incarnation=?2 AND known=0 ORDER BY token LIMIT ?3"))?
          .query_map(params![key.agent,key.incarnation,missing_budget+1],|r|r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        if !missing.is_empty() {
            if missing.len() > BOUND && missing_budget == BOUND {
                fence(tx, ns, "launch missing-token repair page exceeds bound")?;
            }
            let used = missing.len().min(missing_budget);
            page.missing.extend(missing.into_iter().take(used));
            missing_budget -= used;
            if missing_budget == 0 {
                break;
            }
            continue;
        }
        let selected:Option<(String,String)>=tx.query_row(&sql(ns,"SELECT token,member FROM local_agent_launch_candidates WHERE namespace=@NS@ AND agent=?1 AND incarnation=?2 AND member IS NOT NULL ORDER BY ordering DESC,source,id LIMIT 1"),params![key.agent,key.incarnation],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        let selected = selected
            .map(|(token, member)| serde_json::from_str::<MemberSpec>(&member).map(|m| (token, m)))
            .transpose()?;
        let body = serde_json::to_string(&selected)?;
        let prior:Option<String>=tx.query_row(&sql(ns,"SELECT body FROM local_agent_launch_rows WHERE namespace=@NS@ AND agent=?1 AND incarnation=?2"),params![key.agent,key.incarnation],|r|r.get(0)).optional()?;
        if prior.as_deref() != Some(&body) {
            tx.execute(&sql(ns,"INSERT INTO local_agent_launch_rows VALUES(@NS@,?1,?2,?3) ON CONFLICT(namespace,agent,incarnation) DO UPDATE SET body=excluded.body"),params![key.agent,key.incarnation,body])?;
            page.changed.insert(key.clone());
        }
        tx.execute(&sql(ns,"DELETE FROM local_agent_launch_dirty WHERE namespace=@NS@ AND agent=?1 AND incarnation=?2"),params![key.agent,key.incarnation])?;
    }
    page.complete = !pending(tx, ns)?;
    Ok(page)
}
fn pending(c: &Connection, ns: &Namespace) -> Result<bool> {
    Ok(c.query_row(&sql(ns,"SELECT EXISTS(SELECT 1 FROM local_agent_launch_dirty WHERE namespace=@NS@) OR EXISTS(SELECT 1 FROM local_agent_launch_fences WHERE namespace=@NS@)"),[],|r|r.get(0))?)
}
/// Dependency closure only; full source extraction/cut and authorization remain external.
pub(crate) fn ensure_closed(c: &Connection, ns: &Namespace) -> Result<()> {
    anyhow::ensure!(!pending(c, ns)?, "launch namespace pending or fenced");
    Ok(())
}
/// Indexed sparse-cache read, only in a fully source-certified authorized snapshot. A
/// missing cache key means no captured candidate, not proof of source extraction completion.
pub(crate) fn read_selected(
    c: &Connection,
    ns: &Namespace,
    key: &Key,
) -> Result<Option<(String, MemberSpec)>> {
    let blocked:bool=c.query_row(&sql(ns,"SELECT EXISTS(SELECT 1 FROM local_agent_launch_fences WHERE namespace=@NS@) OR EXISTS(SELECT 1 FROM local_agent_launch_dirty WHERE namespace=@NS@ AND agent=?1 AND incarnation=?2)"),params![key.agent,key.incarnation],|r|r.get(0))?;
    anyhow::ensure!(!blocked, "launch pair pending or namespace fenced");
    let body:Option<String>=c.query_row(&sql(ns,"SELECT body FROM local_agent_launch_rows WHERE namespace=@NS@ AND agent=?1 AND incarnation=?2"),params![key.agent,key.incarnation],|r|r.get(0)).optional()?;
    body.map(|b| serde_json::from_str(&b))
        .transpose()
        .map(Option::flatten)
        .map_err(Into::into)
}
/// At most limit total rows across five tables; unified owner shares its overall budget.
pub(crate) fn reclaim_namespace(
    tx: &Transaction<'_>,
    ns: &Namespace,
    limit: usize,
) -> Result<bool> {
    anyhow::ensure!(
        (1..=BOUND).contains(&limit),
        "launch reclaim page exceeds bound"
    );
    let ready: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM ivm_install_roots WHERE namespace=?1 AND ready=1)",
        [ns.as_str()],
        |r| r.get(0),
    )?;
    anyhow::ensure!(!ready, "cannot reclaim a ready launch namespace");
    let tables = [
        ("local_agent_launch_tokens", "id"),
        ("local_agent_launch_candidates", "source,id"),
        ("local_agent_launch_dirty", "agent,incarnation"),
        ("local_agent_launch_rows", "agent,incarnation"),
        ("local_agent_launch_fences", "namespace"),
    ];
    let mut remaining = limit;
    for (table, keys) in tables {
        if remaining == 0 {
            break;
        }
        let q = format!(
            "DELETE FROM {table} WHERE namespace=@NS@ AND ({keys}) IN (SELECT {keys} FROM {table} WHERE namespace=@NS@ ORDER BY {keys} LIMIT ?1)"
        );
        remaining -= tx.execute(&sql(ns, &q), [remaining])?;
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
