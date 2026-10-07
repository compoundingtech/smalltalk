//! Unmanaged declaration leaves. Selection is by desired revision and claim ID,
//! rather than canonical arrival order. Owned declarations use the owned-set
//! dependency instead. Missing parents still suppress matching IDs, as in the
//! snapshot-index legacy selector; neither source requires a history read here.
use super::*;
use smallclaims::ivm::install::Namespace;

const MAX_PREDECESSORS: usize = 128;
const MAX_BODY_BYTES: usize = 256 * 1024;
const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS local_agent_card_desired_inputs(
 namespace TEXT NOT NULL,id TEXT NOT NULL,agent TEXT NOT NULL,
 revision TEXT NOT NULL,body TEXT NOT NULL,invalid INTEGER NOT NULL,
 leaf INTEGER NOT NULL,PRIMARY KEY(namespace,id)
);
CREATE INDEX IF NOT EXISTS local_agent_card_desired_selected
 ON local_agent_card_desired_inputs(namespace,agent,leaf,revision DESC,id DESC);
CREATE INDEX IF NOT EXISTS local_agent_card_desired_invalid
 ON local_agent_card_desired_inputs(namespace,agent,leaf,invalid,id);
CREATE TABLE IF NOT EXISTS local_agent_card_desired_refs(
 namespace TEXT NOT NULL,agent TEXT NOT NULL,child TEXT NOT NULL,parent TEXT NOT NULL,
 PRIMARY KEY(namespace,child,parent)
);
CREATE INDEX IF NOT EXISTS local_agent_card_desired_parent
 ON local_agent_card_desired_refs(namespace,agent,parent,child);
"#;

pub(super) fn create_schema(connection: &Connection) -> Result<()> {
    connection.execute_batch(SCHEMA)?;
    Ok(())
}

pub(super) fn apply_claim(
    tx: &Transaction<'_>,
    namespace: &Namespace,
    old: Option<&ClaimRecord>,
    new: Option<&ClaimRecord>,
) -> Result<BTreeSet<String>> {
    apply(tx, namespace.as_str(), old, new)
}

fn update_leaf(tx: &Transaction<'_>, ns: &str, agent: &str, id: &str) -> Result<()> {
    tx.execute("UPDATE local_agent_card_desired_inputs SET leaf=NOT EXISTS(SELECT 1 FROM local_agent_card_desired_refs WHERE namespace=?1 AND agent=?2 AND parent=?3) WHERE namespace=?1 AND agent=?2 AND id=?3",params![ns,agent,id])?;
    Ok(())
}

fn retract(tx: &Transaction<'_>, ns: &str, id: &str) -> Result<Option<String>> {
    let agent: Option<String> = tx
        .query_row(
            "SELECT agent FROM local_agent_card_desired_inputs WHERE namespace=?1 AND id=?2",
            params![ns, id],
            |r| r.get(0),
        )
        .optional()?;
    let parents: Vec<(String,String)> = tx.prepare_cached("SELECT agent,parent FROM local_agent_card_desired_refs WHERE namespace=?1 AND child=?2 LIMIT ?3")?.query_map(params![ns,id,(MAX_PREDECESSORS+1) as i64],|r|Ok((r.get(0)?,r.get(1)?)))?.collect::<Result<_,_>>()?;
    anyhow::ensure!(
        parents.len() <= MAX_PREDECESSORS,
        "unmanaged desired predecessor bound exceeded"
    );
    tx.execute(
        "DELETE FROM local_agent_card_desired_refs WHERE namespace=?1 AND child=?2",
        params![ns, id],
    )?;
    tx.execute(
        "DELETE FROM local_agent_card_desired_inputs WHERE namespace=?1 AND id=?2",
        params![ns, id],
    )?;
    for (agent, parent) in parents {
        update_leaf(tx, ns, &agent, &parent)?;
    }
    Ok(agent)
}

fn apply(
    tx: &Transaction<'_>,
    ns: &str,
    old: Option<&ClaimRecord>,
    new: Option<&ClaimRecord>,
) -> Result<BTreeSet<String>> {
    let mut agents = BTreeSet::new();
    for claim in old.into_iter().chain(new) {
        if let Some(agent) = retract(tx, ns, &claim.id)? {
            agents.insert(agent);
        }
    }
    if let Some(claim) =
        new.filter(|c| c.subject.starts_with("agent/") && c.kind == "intent.desired")
    {
        let body = serde_json::to_string(&claim.body)?;
        anyhow::ensure!(
            body.len() <= MAX_BODY_BYTES,
            "unmanaged desired body bound exceeded"
        );
        anyhow::ensure!(
            claim.predecessors.len() <= MAX_PREDECESSORS,
            "unmanaged desired predecessor bound exceeded"
        );
        let desired = serde_json::from_value::<DesiredSubject>(claim.body.clone());
        let (revision, invalid) = match desired {
            Ok(ref desired) => (desired_revision(desired), false),
            Err(_) => (String::new(), true),
        };
        tx.execute(
            "INSERT INTO local_agent_card_desired_inputs VALUES(?1,?2,?3,?4,?5,?6,1)",
            params![ns, claim.id, claim.subject, revision, body, invalid],
        )?;
        for parent in &claim.predecessors {
            tx.execute(
                "INSERT OR IGNORE INTO local_agent_card_desired_refs VALUES(?1,?2,?3,?4)",
                params![ns, claim.subject, claim.id, parent],
            )?;
            update_leaf(tx, ns, &claim.subject, parent)?;
        }
        update_leaf(tx, ns, &claim.subject, &claim.id)?;
        agents.insert(claim.subject.clone());
    }
    Ok(agents)
}

pub(super) fn read(
    connection: &Connection,
    namespace: &Namespace,
    agent: &str,
) -> Result<Option<(DesiredSubject, String, String)>> {
    selected(connection, namespace.as_str(), agent)
}

fn selected(
    connection: &Connection,
    ns: &str,
    agent: &str,
) -> Result<Option<(DesiredSubject, String, String)>> {
    let invalid:bool=connection.query_row("SELECT EXISTS(SELECT 1 FROM local_agent_card_desired_inputs WHERE namespace=?1 AND agent=?2 AND leaf=1 AND invalid=1)",params![ns,agent],|r|r.get(0))?;
    anyhow::ensure!(!invalid, "invalid unmanaged desired leaf");
    let row:Option<(String,String,String)>=connection.query_row("SELECT body,id,revision FROM local_agent_card_desired_inputs WHERE namespace=?1 AND agent=?2 AND leaf=1 ORDER BY revision DESC,id DESC LIMIT 1",params![ns,agent],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
    row.map(|(body, id, revision)| Ok((serde_json::from_str(&body)?, id, revision)))
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;
    const AGENT: &str = "agent/node.amber";
    fn desired(command: &str) -> Value {
        let source = format!("version 2\nagent \"amber\" {{ command \"{command}\" }}\n");
        let desired = crate::graph::parse_test_intent(&source, "node")
            .unwrap()
            .subjects
            .remove(AGENT)
            .unwrap();
        serde_json::to_value(desired).unwrap()
    }
    fn append(store: &Store, body: Value, parents: &[String]) -> ClaimRecord {
        let mut writer = store.connection.write();
        let tx = writer.transaction().unwrap();
        let claim = smallclaims::store::append_claim_record_tx(
            &tx,
            "node",
            AGENT,
            "intent.desired",
            Some("person/fixture"),
            &body,
            parents,
            None,
        )
        .unwrap();
        apply(&tx, "live", None, Some(&claim)).unwrap();
        tx.commit().unwrap();
        claim
    }
    fn check(store: &Store) {
        let connection = store.readers.get();
        let index = current_index(&connection).unwrap();
        let expected = desired_row_at(&connection, AGENT, Some(index));
        let actual = selected(&connection, "live", AGENT);
        match (expected, actual) {
            (Ok(expected), Ok(actual)) => assert_eq!(
                expected.map(|r| (
                    r.claim_id,
                    r.revision,
                    r.body,
                    r.member,
                    r.owner_run,
                    r.owner_generation
                )),
                actual.map(|(d, id, rev)| (
                    id,
                    rev,
                    canonical_json_text(&d.desired).unwrap(),
                    d.member
                        .as_ref()
                        .map(|m| canonical_serialized_json_text(m).unwrap()),
                    d.owner_run,
                    d.owner_generation
                ))
            ),
            (Err(_), Err(_)) => {}
            (expected, actual) => panic!(
                "selection parity: {} / {}",
                expected.is_ok(),
                actual.is_ok()
            ),
        }
    }
    #[test]
    fn desired_leaves_revision_ties_late_parents_and_retraction_match_store() {
        let store = Store::open_memory("node").unwrap();
        create_schema(&store.connection.write()).unwrap();
        let parent = append(&store, desired("true"), &[]);
        check(&store);
        let child = append(&store, desired("false"), std::slice::from_ref(&parent.id));
        check(&store);
        append(&store, desired("true"), &[]);
        check(&store);
        append(&store, desired("true"), &[]);
        check(&store);
        let mut writer = store.connection.write();
        let tx = writer.transaction().unwrap();
        tx.execute("DELETE FROM claims WHERE id=?1", [&child.id])
            .unwrap();
        apply(&tx, "live", Some(&child), None).unwrap();
        tx.commit().unwrap();
        drop(writer);
        check(&store);
        // Stage a descendant before its parent; later arrival must remain non-leaf.
        let mut writer = store.connection.write();
        let tx = writer.transaction().unwrap();
        apply(&tx, "late", None, Some(&child)).unwrap();
        apply(&tx, "late", None, Some(&parent)).unwrap();
        assert_eq!(selected(&tx, "late", AGENT).unwrap().unwrap().1, child.id);
        tx.rollback().unwrap();
    }
    #[test]
    fn invalid_leaf_suppression_subject_replacement_and_rollback_are_exact() {
        let store = Store::open_memory("node").unwrap();
        create_schema(&store.connection.write()).unwrap();
        let bad = append(&store, json!({"unsupported":true}), &[]);
        check(&store);
        append(&store, desired("true"), std::slice::from_ref(&bad.id));
        check(&store);
        let mut writer = store.connection.write();
        let tx = writer.transaction().unwrap();
        let mut replaced = bad.clone();
        replaced.subject = "agent/node.fern".into();
        // Old image deliberately omitted: staged identity must retract the old subject.
        let affected = apply(&tx, "live", None, Some(&replaced)).unwrap();
        assert!(affected.contains(AGENT));
        assert!(affected.contains(&replaced.subject));
        assert!(selected(&tx, "live", AGENT).is_ok());
        assert!(selected(&tx, "live", &replaced.subject).is_err());
        assert!(selected(&tx, "other", AGENT).unwrap().is_none());
        tx.rollback().unwrap();
        drop(writer);
        check(&store);
    }
    #[test]
    fn desired_bounds_and_indexed_reads_refuse_partial_selection() {
        let store = Store::open_memory("node").unwrap();
        create_schema(&store.connection.write()).unwrap();
        let claim = append(&store, desired("true"), &[]);
        let mut writer = store.connection.write();
        let tx = writer.transaction().unwrap();
        let mut too_many = claim.clone();
        too_many.predecessors = (0..129).map(|i| format!("claim/fixture-{i}")).collect();
        assert!(apply(&tx, "overflow", None, Some(&too_many)).is_err());
        assert!(selected(&tx, "overflow", AGENT).unwrap().is_none());
        let plan:Vec<String>=tx.prepare("EXPLAIN QUERY PLAN SELECT id FROM local_agent_card_desired_inputs WHERE namespace=?1 AND agent=?2 AND leaf=1 ORDER BY revision DESC,id DESC LIMIT 1").unwrap().query_map(params!["live",AGENT],|r|r.get(3)).unwrap().collect::<Result<_,_>>().unwrap();
        assert!(
            plan.iter()
                .any(|p| p.contains("local_agent_card_desired_selected"))
        );
        assert!(!plan.iter().any(|p| p.contains("TEMP B-TREE")));
        tx.rollback().unwrap();
    }
}
