//! Namespace-owned activity heads and canonical working-episode boundaries.
//! Replica-local arrival selection is intentional for activity. Capture must
//! redispatch store-index renumbering and local prefix promotions at a new epoch.
//! These internal inputs never establish public-card readiness on their own.

use super::*;
use smallclaims::ivm::install::Namespace;

const CONTENT: &[&str] = &["message", "content", "tool_call", "tool_result"];
const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS local_agent_card_activity_inputs (
 namespace TEXT NOT NULL, source INTEGER NOT NULL, claim TEXT NOT NULL,
 category TEXT NOT NULL, agent TEXT NOT NULL, incarnation TEXT NOT NULL,
 position INTEGER NOT NULL, after_index INTEGER NOT NULL, at TEXT NOT NULL, eligible INTEGER NOT NULL,
 PRIMARY KEY(namespace,source,claim,category)
);
CREATE INDEX IF NOT EXISTS local_agent_card_activity_head
 ON local_agent_card_activity_inputs(namespace,agent,incarnation,category,source,position DESC,claim DESC);
CREATE TABLE IF NOT EXISTS local_agent_card_activity_cut(namespace TEXT PRIMARY KEY,snapshot_index INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS local_agent_card_activity_current_head
 ON local_agent_card_activity_inputs(namespace,agent,incarnation,category,source,eligible,position DESC,claim DESC);
CREATE INDEX IF NOT EXISTS local_agent_card_activity_local_cut
 ON local_agent_card_activity_inputs(namespace,source,after_index,claim,agent);
CREATE TABLE IF NOT EXISTS local_agent_card_working_inputs (
 namespace TEXT NOT NULL, claim TEXT NOT NULL, agent TEXT NOT NULL,
 incarnation TEXT NOT NULL, working INTEGER NOT NULL, rank BLOB NOT NULL, at TEXT NOT NULL,
 PRIMARY KEY(namespace,claim)
);
CREATE INDEX IF NOT EXISTS local_agent_card_working_head
 ON local_agent_card_working_inputs(namespace,agent,incarnation,working,rank,claim);
"#;

pub(super) fn create_schema(connection: &Connection) -> Result<()> {
    connection.execute_batch(SCHEMA)?;
    Ok(())
}

pub(super) fn apply_claim(
    tx: &Transaction<'_>,
    namespace: &Namespace,
    old: Option<&ClaimRecord>,
    new: Option<(&ClaimRecord, &canonical::ClaimKey)>,
) -> Result<BTreeSet<String>> {
    apply(tx, namespace.as_str(), old, new)
}

fn apply(
    tx: &Transaction<'_>,
    namespace: &str,
    old: Option<&ClaimRecord>,
    new: Option<(&ClaimRecord, &canonical::ClaimKey)>,
) -> Result<BTreeSet<String>> {
    let mut agents = BTreeSet::new();
    // A scan page may have seen a newer replacement than the journal entry
    // being replayed. Retract the namespace's actual stored dependency keys.
    for claim in old.into_iter().chain(new.map(|(claim, _)| claim)) {
        let mut query = tx.prepare("SELECT agent FROM local_agent_card_activity_inputs WHERE namespace=?1 AND source=0 AND claim=?2 LIMIT 3")?;
        agents.extend(
            query
                .query_map(params![namespace, claim.id], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?,
        );
        if let Some(agent) = tx
            .query_row(
                "SELECT agent FROM local_agent_card_working_inputs WHERE namespace=?1 AND claim=?2",
                params![namespace, claim.id],
                |r| r.get::<_, String>(0),
            )
            .optional()?
        {
            agents.insert(agent);
        }
        tx.execute("DELETE FROM local_agent_card_activity_inputs WHERE namespace=?1 AND source=0 AND claim=?2",params![namespace,claim.id])?;
        tx.execute(
            "DELETE FROM local_agent_card_working_inputs WHERE namespace=?1 AND claim=?2",
            params![namespace, claim.id],
        )?;
    }
    if let Some(old) = old {
        for (agent, _, _) in activity_keys(old) {
            agents.insert(agent);
        }
        if old.subject.starts_with("agent/") {
            agents.insert(old.subject.clone());
        }
        tx.execute("DELETE FROM local_agent_card_activity_inputs WHERE namespace=?1 AND source=0 AND claim=?2",params![namespace,old.id])?;
        tx.execute(
            "DELETE FROM local_agent_card_working_inputs WHERE namespace=?1 AND claim=?2",
            params![namespace, old.id],
        )?;
    }
    if let Some((claim, key)) = new {
        for (agent, incarnation, category) in activity_keys(claim) {
            tx.execute("INSERT INTO local_agent_card_activity_inputs VALUES(?1,0,?2,?3,?4,?5,?6,?6,?7,1) ON CONFLICT(namespace,source,claim,category) DO UPDATE SET agent=excluded.agent,incarnation=excluded.incarnation,position=excluded.position,after_index=excluded.after_index,at=excluded.at",params![namespace,claim.id,category,agent,serde_json::to_string(&incarnation)?,claim.store_index,claim.accepted_at_unix_ms.to_string()])?;
            agents.insert(agent);
        }
        if claim.kind == "harness.observed" && claim.subject.starts_with("agent/") {
            let fields = &claim.body["fields"];
            if let Some(incarnation) = fields["incarnation_id"].as_str() {
                let working = match fields.get("state") {
                    None | Some(Value::Null) => None,
                    Some(Value::String(state)) => Some(state == "working"),
                    Some(Value::Array(_) | Value::Object(_)) => Some(false),
                    Some(_) => anyhow::bail!("non-text scalar working episode state"),
                };
                if let Some(working) = working {
                    tx.execute("INSERT INTO local_agent_card_working_inputs VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(namespace,claim) DO UPDATE SET agent=excluded.agent,incarnation=excluded.incarnation,working=excluded.working,rank=excluded.rank,at=excluded.at",params![namespace,claim.id,claim.subject,incarnation,working,canonical::sortable_key(key),claim.accepted_at_unix_ms.to_string()])?;
                    agents.insert(claim.subject.clone());
                }
            }
        }
    }
    Ok(agents)
}

fn activity_keys(claim: &ClaimRecord) -> Vec<(String, Option<String>, &'static str)> {
    let mut keys = Vec::new();
    let mut add = |agent: Option<&str>, incarnation: Option<&str>, category| {
        if let Some(agent) = agent.filter(|agent| agent.starts_with("agent/")) {
            keys.push((agent.to_owned(), incarnation.map(str::to_owned), category));
        }
    };
    match claim.kind.as_str() {
        "work.progress" | "work.submitted" => add(claim.actor.as_deref(), None, "work"),
        "message.sent" => {
            add(claim.body["fields"]["from"].as_str(), None, "sent");
            add(claim.body["fields"]["to"].as_str(), None, "received");
        }
        "harness.timeline"
            if claim.body["fields"]["entry_type"]
                .as_str()
                .is_some_and(|kind| CONTENT.contains(&kind)) =>
        {
            if let Some(incarnation) = claim.body["fields"]["incarnation_id"].as_str() {
                add(Some(&claim.subject), Some(incarnation), "content");
            }
        }
        _ => {}
    }
    keys
}

/// The complete captured local row, represented by the normal local ClaimRecord.
/// Position is its numeric observation ID, not the after_store_index anchor.
/// Capture retractions/replacements before insertion and all changed old/new keys.
pub(super) fn apply_local(
    tx: &Transaction<'_>,
    namespace: &Namespace,
    old: Option<&ClaimRecord>,
    new: Option<&ClaimRecord>,
) -> Result<BTreeSet<String>> {
    local(tx, namespace.as_str(), old, new)
}

fn local(
    tx: &Transaction<'_>,
    namespace: &str,
    old: Option<&ClaimRecord>,
    new: Option<&ClaimRecord>,
) -> Result<BTreeSet<String>> {
    let mut agents = BTreeSet::new();
    for claim in old.into_iter().chain(new) {
        if let Some(agent) = tx.query_row("SELECT agent FROM local_agent_card_activity_inputs WHERE namespace=?1 AND source=1 AND claim=?2 LIMIT 1",params![namespace,claim.id],|r|r.get::<_,String>(0)).optional()? { agents.insert(agent); }
        tx.execute("DELETE FROM local_agent_card_activity_inputs WHERE namespace=?1 AND source=1 AND claim=?2",params![namespace,claim.id])?;
    }
    if let Some(old) = old {
        tx.execute("DELETE FROM local_agent_card_activity_inputs WHERE namespace=?1 AND source=1 AND claim=?2",params![namespace,old.id])?;
        for (agent, _, _) in activity_keys(old) {
            agents.insert(agent);
        }
    }
    if let Some(claim) = new.filter(|claim| claim.kind == "harness.timeline") {
        let position = local_observation_position(claim)
            .context("local activity input missing observation identity")?;
        let cut: Option<u64> = tx
            .query_row(
                "SELECT snapshot_index FROM local_agent_card_activity_cut WHERE namespace=?1",
                [namespace],
                |r| r.get(0),
            )
            .optional()?;
        let eligible = cut.is_none_or(|cut| claim.store_index <= cut);
        for (agent, incarnation, category) in activity_keys(claim) {
            tx.execute("INSERT INTO local_agent_card_activity_inputs VALUES(?1,1,?2,?3,?4,?5,?6,?7,?8,?9) ON CONFLICT(namespace,source,claim,category) DO UPDATE SET agent=excluded.agent,incarnation=excluded.incarnation,position=excluded.position,after_index=excluded.after_index,at=excluded.at,eligible=excluded.eligible",params![namespace,claim.id,category,agent,serde_json::to_string(&incarnation)?,position,claim.store_index,claim.accepted_at_unix_ms.to_string(),eligible])?;
            agents.insert(agent);
        }
    }
    Ok(agents)
}

pub(super) fn set_captured_cut(
    tx: &Transaction<'_>,
    ns: &Namespace,
    snapshot_index: u64,
) -> Result<()> {
    tx.execute("INSERT INTO local_agent_card_activity_cut VALUES(?1,?2) ON CONFLICT(namespace) DO UPDATE SET snapshot_index=excluded.snapshot_index",params![ns.as_str(),snapshot_index])?;
    Ok(())
}
pub(super) fn repair_local_cut(
    tx: &Transaction<'_>,
    ns: &Namespace,
    claim: &str,
    snapshot_index: u64,
) -> Result<()> {
    tx.execute("UPDATE local_agent_card_activity_inputs SET eligible=(after_index<=?3) WHERE namespace=?1 AND source=1 AND claim=?2",params![ns.as_str(),claim,snapshot_index])?;
    Ok(())
}
/// Current-state namespace head, only after the owner's indexed local cut repair closes.
/// Historical oracle tests retain an explicit cut filter; this head serves current state only.
pub(super) fn current_activity(
    c: &Connection,
    ns: &Namespace,
    agent: &str,
    incarnation: Option<&str>,
) -> Result<Option<u128>> {
    let mut latest = None;
    for (category, incarnation, source) in [
        ("work", None, 0),
        ("sent", None, 0),
        ("received", None, 0),
        ("content", incarnation, 0),
        ("content", incarnation, 1),
    ] {
        if category == "content" && incarnation.is_none() {
            continue;
        }
        let at:Option<String>=c.query_row("SELECT at FROM local_agent_card_activity_inputs INDEXED BY local_agent_card_activity_current_head WHERE namespace=?1 AND agent=?2 AND incarnation=?3 AND category=?4 AND source=?5 AND eligible=1 ORDER BY position DESC,claim DESC LIMIT 1",params![ns.as_str(),agent,serde_json::to_string(&incarnation)?,category,source],|r|r.get(0)).optional()?;
        if let Some(at) = at {
            let at: u128 = at.parse()?;
            latest = Some(latest.map_or(at, |prior: u128| prior.max(at)));
        }
    }
    Ok(latest)
}

pub(super) type LocalCutRow = (u64, String, String);

/// Indexed invalidation when the captured native claim position crosses local anchors.
/// This predicate input establishes no projected prefix or readiness on its own. The owner
/// retains the range/cursor transactionally, shares its budget, and rejects incomplete repair.
pub(super) fn local_cut_page(
    c: &Connection,
    ns: &Namespace,
    lower: u64,
    upper: u64,
    after: Option<(u64, &str)>,
    limit: usize,
) -> Result<(Vec<LocalCutRow>, bool)> {
    anyhow::ensure!(
        (1..=128).contains(&limit) && lower <= upper && upper <= i64::MAX as u64,
        "local activity cut page bounds"
    );
    let (at, id) = after.unwrap_or((lower, ""));
    anyhow::ensure!(
        lower <= at && at <= upper,
        "local activity cut continuation"
    );
    // Keep the continuation as the index range itself, rather than a residual predicate
    // behind the original lower bound. This also bounds repeated equal-anchor pages.
    let sql = if id.is_empty() {
        "SELECT after_index,claim,agent FROM local_agent_card_activity_inputs INDEXED BY local_agent_card_activity_local_cut WHERE namespace=?1 AND source=1 AND after_index>?2 AND after_index<=?3 ORDER BY after_index,claim LIMIT ?6"
    } else {
        "SELECT after_index,claim,agent FROM local_agent_card_activity_inputs INDEXED BY local_agent_card_activity_local_cut WHERE namespace=?1 AND source=1 AND (after_index,claim)>(?4,?5) AND after_index<=?3 ORDER BY after_index,claim LIMIT ?6"
    };
    let mut rows: Vec<(u64, String, String)> = c
        .prepare_cached(sql)?
        .query_map(params![ns.as_str(), lower, upper, at, id, limit + 1], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let more = rows.len() > limit;
    rows.truncate(limit);
    Ok((rows, more))
}

#[cfg(test)]
fn read_activity(
    connection: &Connection,
    namespace: &str,
    agent: &str,
    incarnation: Option<&str>,
    projected: u64,
) -> Result<Option<u128>> {
    let mut latest = None;
    // Each category first selects its newest receipt, then timestamps are maxed.
    // MAX(at) across all historical receipts would change the legacy behavior.
    for (category, incarnation, source) in [
        ("work", None, 0),
        ("sent", None, 0),
        ("received", None, 0),
        ("content", incarnation, 0),
        ("content", incarnation, 1),
    ] {
        if category == "content" && incarnation.is_none() {
            continue;
        }
        let at: Option<String> = connection.query_row("SELECT at FROM local_agent_card_activity_inputs WHERE namespace=?1 AND agent=?2 AND incarnation=?3 AND category=?4 AND source=?5 AND after_index<=?6 ORDER BY position DESC,claim DESC LIMIT 1",params![namespace,agent,serde_json::to_string(&incarnation)?,category,source,projected],|r|r.get(0)).optional()?;
        if let Some(at) = at {
            let at: u128 = at.parse()?;
            latest = Some(latest.map_or(at, |previous: u128| previous.max(at)));
        }
    }
    Ok(latest)
}

pub(super) fn working_since(
    connection: &Connection,
    namespace: &Namespace,
    agent: &str,
    incarnation: &str,
) -> Result<Option<u128>> {
    read_working(connection, namespace.as_str(), agent, incarnation)
}

fn read_working(
    connection: &Connection,
    namespace: &str,
    agent: &str,
    incarnation: &str,
) -> Result<Option<u128>> {
    let boundary: Option<Vec<u8>> = connection.query_row("SELECT rank FROM local_agent_card_working_inputs WHERE namespace=?1 AND agent=?2 AND incarnation=?3 AND working=0 ORDER BY rank DESC,claim DESC LIMIT 1",params![namespace,agent,incarnation],|r|r.get(0)).optional()?;
    let at: Option<String> = connection.query_row("SELECT at FROM local_agent_card_working_inputs WHERE namespace=?1 AND agent=?2 AND incarnation=?3 AND working=1 AND rank>?4 ORDER BY rank,claim LIMIT 1",params![namespace,agent,incarnation,boundary.unwrap_or_default()],|r|r.get(0)).optional()?;
    at.map(|value| Ok(value.parse()?)).transpose()
}

#[cfg(test)]
mod tests {
    use super::*;
    const AGENT: &str = "agent/grove.cedar";
    const OTHER: &str = "agent/grove.birch";

    struct Fixture {
        store: Store,
    }
    impl Fixture {
        fn new() -> Self {
            let store = Store::open_memory("grove").unwrap();
            create_schema(&store.connection.write()).unwrap();
            Self { store }
        }
        fn append(
            &self,
            origin: &str,
            subject: &str,
            kind: &str,
            actor: Option<&str>,
            fields: Value,
            time: u128,
        ) -> ClaimRecord {
            self.store.set_write_clock_at(time).unwrap();
            let mut connection = self.store.connection.write();
            let tx = connection.transaction().unwrap();
            let claim = smallclaims::store::append_claim_record_tx(
                &tx,
                origin,
                subject,
                kind,
                actor,
                &json!({"fields":fields}),
                &[],
                None,
            )
            .unwrap();
            let key = canonical::claim_key(&tx, &claim.id).unwrap();
            apply(&tx, "live", None, Some((&claim, &key))).unwrap();
            tx.commit().unwrap();
            drop(connection);
            self.check();
            claim
        }
        fn check(&self) {
            let connection = self.store.readers.get();
            let index = current_index(&connection).unwrap();
            for agent in [AGENT, OTHER] {
                for incarnation in [None, Some("one"), Some("two"), Some("")] {
                    assert_eq!(
                        read_activity(&connection, "live", agent, incarnation, index).unwrap(),
                        self.store
                            .agent_last_activity_at(agent, incarnation, index)
                            .unwrap(),
                        "activity {agent}/{incarnation:?}"
                    );
                    if let Some(incarnation) = incarnation {
                        assert_eq!(
                            read_working(&connection, "live", agent, incarnation).unwrap(),
                            agent_working_since_at(&connection, agent, incarnation, index).unwrap(),
                            "working {agent}/{incarnation}"
                        );
                    }
                }
            }
        }
        fn observation(
            &self,
            agent: &str,
            incarnation: &str,
            entry: &str,
            time: i64,
        ) -> ClaimRecord {
            let mut connection = self.store.connection.write();
            let tx = connection.transaction().unwrap();
            let index = current_index(&tx).unwrap();
            tx.execute("INSERT INTO local_observations(after_store_index,subject,kind,body,observed_at_unix_ms) VALUES(?1,?2,'harness.timeline',?3,?4)",params![index,agent,json!({"fields":{"incarnation_id":incarnation,"entry_type":entry}}).to_string(),time]).unwrap();
            let id = tx.last_insert_rowid();
            let claim = tx.query_row("SELECT id,after_store_index,subject,kind,actor,body,observed_at_unix_ms FROM local_observations WHERE id=?1",[id],|r|local_observation_from_row("grove",r)).unwrap();
            local(&tx, "live", None, Some(&claim)).unwrap();
            tx.commit().unwrap();
            drop(connection);
            self.check();
            claim
        }
    }

    #[test]
    fn arrival_categories_local_replacements_and_incarnations_match_store() {
        let f = Fixture::new();
        f.append(
            "grove",
            AGENT,
            "harness.timeline",
            None,
            json!({"incarnation_id":"one","entry_type":"message"}),
            900,
        );
        f.append(
            "grove",
            "step/example",
            "work.progress",
            Some(AGENT),
            json!({}),
            800,
        );
        f.append(
            "grove",
            "message/example",
            "message.sent",
            None,
            json!({"from":AGENT,"to":OTHER}),
            700,
        );
        // Later arrival with an older writer timestamp replaces its category head.
        f.append(
            "remote",
            AGENT,
            "harness.timeline",
            None,
            json!({"incarnation_id":"one","entry_type":"content"}),
            100,
        );
        f.append(
            "grove",
            AGENT,
            "harness.timeline",
            None,
            json!({"incarnation_id":"two","entry_type":"status"}),
            1000,
        );
        f.observation(AGENT, "one", "tool_call", 1200);
        let latest = f.observation(AGENT, "one", "tool_result", 200);
        f.observation(AGENT, "two", "content", 1300);
        f.observation(OTHER, "", "message", -10);
        let mut connection = f.store.connection.write();
        let tx = connection.transaction().unwrap();
        tx.execute(
            "DELETE FROM local_observations WHERE id=?1",
            [local_observation_position(&latest).unwrap()],
        )
        .unwrap();
        local(&tx, "live", Some(&latest), None).unwrap();
        tx.commit().unwrap();
        drop(connection);
        f.check();
    }

    #[test]
    fn canonical_working_suffix_skips_null_and_late_boundary_retracts() {
        let f = Fixture::new();
        f.append(
            "grove",
            AGENT,
            "harness.observed",
            None,
            json!({"incarnation_id":"one","state":"idle"}),
            100,
        );
        f.append(
            "grove",
            AGENT,
            "harness.observed",
            None,
            json!({"incarnation_id":"one","state":"working"}),
            200,
        );
        f.append(
            "grove",
            AGENT,
            "harness.observed",
            None,
            json!({"incarnation_id":"one","state":null}),
            300,
        );
        f.append(
            "grove",
            AGENT,
            "harness.observed",
            None,
            json!({"incarnation_id":"one","state":"working"}),
            400,
        );
        let boundary = f.append(
            "remote",
            AGENT,
            "harness.observed",
            None,
            json!({"incarnation_id":"one","state":"blocked"}),
            250,
        );
        let connection = f.store.readers.get();
        assert_eq!(
            read_working(&connection, "live", AGENT, "one").unwrap(),
            Some(400)
        );
        drop(connection);
        let mut connection = f.store.connection.write();
        let tx = connection.transaction().unwrap();
        // Isolate retraction behavior; mutation certification is the shared source owner.
        tx.execute("DELETE FROM claims WHERE id=?1", [&boundary.id])
            .unwrap();
        apply(&tx, "live", Some(&boundary), None).unwrap();
        tx.commit().unwrap();
        drop(connection);
        f.check();
        assert_eq!(
            read_working(&f.store.readers.get(), "live", AGENT, "one").unwrap(),
            Some(200)
        );
    }

    #[test]
    fn namespaces_and_raced_scan_replacement_invalidate_actual_old_keys() {
        let f = Fixture::new();
        let old = f.append(
            "grove",
            "message/example",
            "message.sent",
            None,
            json!({"from":AGENT,"to":OTHER}),
            100,
        );
        let connection = f.store.readers.get();
        let key = canonical::claim_key(&connection, &old.id).unwrap();
        drop(connection);
        let mut connection = f.store.connection.write();
        let tx = connection.transaction().unwrap();
        let mut replacement = old.clone();
        replacement.body = json!({"fields":{"from":OTHER,"to":"person/avery"}});
        apply(&tx, "staging", None, Some((&replacement, &key))).unwrap();
        let dirty = apply(&tx, "staging", None, Some((&old, &key))).unwrap();
        assert_eq!(dirty, BTreeSet::from([AGENT.to_owned(), OTHER.to_owned()]));
        assert_eq!(
            read_activity(&tx, "staging", AGENT, None, old.store_index).unwrap(),
            Some(100)
        );
        assert_eq!(
            read_activity(&tx, "live", AGENT, None, old.store_index).unwrap(),
            Some(100)
        );
        apply(&tx, "staging", Some(&old), None).unwrap();
        assert_eq!(
            read_activity(&tx, "staging", AGENT, None, old.store_index).unwrap(),
            None
        );
        assert_eq!(
            read_activity(&tx, "live", AGENT, None, old.store_index).unwrap(),
            Some(100)
        );
        tx.rollback().unwrap();
    }
}
