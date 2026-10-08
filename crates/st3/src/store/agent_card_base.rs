//! Captured declarations, owner state and public membership for agent cards.
//! The declaration selector supplies complete owned/unmanaged selection facts;
//! a projected `desired` row alone is not that proof. Owner state is the exact
//! current canonical claim fold used by the snapshot-index public reader.
use super::*;
use smallclaims::ivm::install::Namespace;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS local_agent_card_base_claims(
 namespace TEXT NOT NULL,id TEXT NOT NULL,subject TEXT NOT NULL,kind TEXT NOT NULL,
 rank BLOB NOT NULL,at TEXT NOT NULL,operation TEXT,unknown INTEGER NOT NULL,
 PRIMARY KEY(namespace,id)
);
CREATE INDEX IF NOT EXISTS local_agent_card_base_provenance
 ON local_agent_card_base_claims(namespace,subject,rank DESC,id DESC);
CREATE INDEX IF NOT EXISTS local_agent_card_base_unknown
 ON local_agent_card_base_claims(namespace,subject,unknown,kind,id);
CREATE INDEX IF NOT EXISTS local_agent_card_base_operation
 ON local_agent_card_base_claims(namespace,operation,subject,id);
CREATE TABLE IF NOT EXISTS local_agent_card_owner_fields(
 namespace TEXT NOT NULL,subject TEXT NOT NULL,field TEXT NOT NULL,
 id TEXT NOT NULL,rank BLOB NOT NULL,value TEXT,
 PRIMARY KEY(namespace,subject,field,id)
);
CREATE INDEX IF NOT EXISTS local_agent_card_owner_field_head
 ON local_agent_card_owner_fields(namespace,subject,field,rank DESC,id DESC);
CREATE INDEX IF NOT EXISTS local_agent_card_owner_field_source
 ON local_agent_card_owner_fields(namespace,id);
CREATE TABLE IF NOT EXISTS local_agent_card_base_declarations(
 namespace TEXT NOT NULL,agent TEXT NOT NULL,PRIMARY KEY(namespace,agent)
);
CREATE TABLE IF NOT EXISTS local_agent_card_base_desired(
 namespace TEXT NOT NULL,agent TEXT NOT NULL,token TEXT NOT NULL,revision TEXT NOT NULL,
 body TEXT NOT NULL,PRIMARY KEY(namespace,agent)
);
CREATE TABLE IF NOT EXISTS local_agent_card_owner_dependents(
 namespace TEXT NOT NULL,owner TEXT NOT NULL,agent TEXT NOT NULL,
 PRIMARY KEY(namespace,owner,agent)
);
CREATE INDEX IF NOT EXISTS local_agent_card_owner_agent
 ON local_agent_card_owner_dependents(namespace,agent,owner);
CREATE TABLE IF NOT EXISTS local_agent_card_owner_dirty(
 namespace TEXT NOT NULL,owner TEXT NOT NULL,after TEXT NOT NULL,
 PRIMARY KEY(namespace,owner)
);
CREATE TABLE IF NOT EXISTS local_agent_card_base_conflicts(
 namespace TEXT NOT NULL,agent TEXT NOT NULL,operation TEXT NOT NULL,PRIMARY KEY(namespace,agent,operation)
);
CREATE TABLE IF NOT EXISTS local_agent_card_base_missing_operations(
 namespace TEXT NOT NULL,agent TEXT NOT NULL,operation TEXT NOT NULL,PRIMARY KEY(namespace,agent,operation)
);
CREATE TABLE IF NOT EXISTS local_agent_card_operation_states(
 namespace TEXT NOT NULL,id TEXT NOT NULL,state TEXT NOT NULL,PRIMARY KEY(namespace,id)
);
CREATE INDEX IF NOT EXISTS local_agent_card_operation_conflicts
 ON local_agent_card_operation_states(namespace,state,id);
"#;

pub(super) fn create_schema(connection: &Connection) -> Result<()> {
    connection.execute_batch(SCHEMA)?;
    Ok(())
}

fn actual(kind: &str) -> bool {
    !("harness.".."harness/").contains(&kind)
        && !matches!(
            kind,
            "intent.desired" | "runtime.readiness-deadline-reached" | "reconcile.fault"
        )
        && !kind
            .get(..8)
            .is_some_and(|k| k.eq_ignore_ascii_case("harness."))
}

fn dirty_owner(tx: &Transaction<'_>, namespace: &str, owner: &str) -> Result<()> {
    tx.execute("INSERT INTO local_agent_card_owner_dirty VALUES(?1,?2,'') ON CONFLICT(namespace,owner) DO UPDATE SET after=''",params![namespace,owner])?;
    Ok(())
}

fn operation_agent(
    tx: &Transaction<'_>,
    namespace: &str,
    operation: &str,
    agent: &str,
) -> Result<()> {
    let state: Option<String> = tx
        .query_row(
            "SELECT state FROM local_agent_card_operation_states WHERE namespace=?1 AND id=?2",
            params![namespace, operation],
            |r| r.get(0),
        )
        .optional()?;
    if state.is_none() {
        tx.execute(
            "INSERT OR IGNORE INTO local_agent_card_base_missing_operations VALUES(?1,?2,?3)",
            params![namespace, agent, operation],
        )?;
    } else {
        tx.execute("DELETE FROM local_agent_card_base_missing_operations WHERE namespace=?1 AND agent=?2 AND operation=?3",params![namespace,agent,operation])?;
    }
    if state.as_deref() == Some("conflict") {
        tx.execute(
            "INSERT OR IGNORE INTO local_agent_card_base_conflicts VALUES(?1,?2,?3)",
            params![namespace, agent, operation],
        )?;
    } else {
        tx.execute("DELETE FROM local_agent_card_base_conflicts WHERE namespace=?1 AND agent=?2 AND operation=?3",params![namespace,agent,operation])?;
    }
    Ok(())
}

/// All claim mutations carry the receiver's repaired eligibility and canonical
/// metadata. Replacement retracts the actual staged value even if the source's
/// old value differs because scan and journal capture raced.
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
    let mut affected = BTreeSet::new();
    for claim in old.into_iter().chain(new.map(|(c, _)| c)) {
        let stored: Option<(String, Option<String>)> = tx
            .query_row(
                "SELECT subject,operation FROM local_agent_card_base_claims WHERE namespace=?1 AND id=?2",
                params![namespace, claim.id],
                |r| Ok((r.get(0)?,r.get(1)?)),
            )
            .optional()?;
        for subject in stored
            .as_ref()
            .map(|(s, _)| s.clone())
            .into_iter()
            .chain(std::iter::once(claim.subject.clone()))
        {
            if subject.starts_with("agent/") {
                affected.insert(subject);
            } else if subject.starts_with("mission-run/") || subject.starts_with("run-generation/")
            {
                dirty_owner(tx, namespace, &subject)?;
            }
        }
        tx.execute(
            "DELETE FROM local_agent_card_owner_fields WHERE namespace=?1 AND id=?2",
            params![namespace, claim.id],
        )?;
        tx.execute(
            "DELETE FROM local_agent_card_base_claims WHERE namespace=?1 AND id=?2",
            params![namespace, claim.id],
        )?;
        if let Some((subject, Some(operation))) = stored {
            let remains: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM local_agent_card_base_claims WHERE namespace=?1 AND operation=?2 AND subject=?3)",params![namespace,operation,subject],|r|r.get(0))?;
            if !remains {
                tx.execute("DELETE FROM local_agent_card_owner_dependents WHERE namespace=?1 AND owner=?2 AND agent=?3",params![namespace,format!("#operation:{operation}"),subject])?;
                tx.execute("DELETE FROM local_agent_card_base_conflicts WHERE namespace=?1 AND agent=?2 AND operation=?3",params![namespace,subject,operation])?;
                tx.execute("DELETE FROM local_agent_card_base_missing_operations WHERE namespace=?1 AND agent=?2 AND operation=?3",params![namespace,subject,operation])?;
            }
        }
    }
    if let Some((claim, key)) = new
        && (claim.subject.starts_with("agent/")
            || claim.subject.starts_with("mission-run/")
            || claim.subject.starts_with("run-generation/"))
    {
        let fields = claim.body.get("fields").unwrap_or(&claim.body);
        if actual(&claim.kind) {
            anyhow::ensure!(
                fields.get("fields").is_none(),
                "ambiguous nested actual fields source"
            );
            // The opener-specific resource merge carries state not represented
            // by fixed owner/card heads. It needs an explicit compatible source.
            anyhow::ensure!(
                !(claim.kind == "resource.observed"
                    && fields["attribution_only"] == true
                    && fields["kind"].as_str().is_some_and(carries_opener)),
                "attribution-only owner source unsupported"
            );
        }
        let rank = canonical::sortable_key(key);
        let operation = claim.body.pointer("/_operation/id").and_then(Value::as_str);
        tx.execute(
            "INSERT INTO local_agent_card_base_claims VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            params![
                namespace,
                claim.id,
                claim.subject,
                claim.kind,
                rank,
                claim.accepted_at_unix_ms.to_string(),
                operation,
                !known_replicated_claim_kind(&claim.kind)
            ],
        )?;
        if claim.subject.starts_with("agent/")
            && let Some(operation) = operation
        {
            tx.execute(
                "INSERT OR IGNORE INTO local_agent_card_owner_dependents VALUES(?1,?2,?3)",
                params![namespace, format!("#operation:{operation}"), claim.subject],
            )?;
            operation_agent(tx, namespace, operation, &claim.subject)?;
        }
        if !claim.subject.starts_with("agent/")
            && actual(&claim.kind)
            && let Some(fields) = fields.as_object()
        {
            let spec = st3_schema::registry().claim(&claim.kind);
            for field in ["status", "mode"] {
                let reset = claim.kind != "resource.observed"
                    && spec.is_some_and(|s| {
                        s.cardinality == st3_schema::Cardinality::StateTransition
                            && s.fields.contains_key(field)
                    });
                if fields.contains_key(field) || reset {
                    let value = fields.get(field).map(serde_json::to_string).transpose()?;
                    tx.execute(
                        "INSERT INTO local_agent_card_owner_fields VALUES(?1,?2,?3,?4,?5,?6)",
                        params![namespace, claim.subject, field, claim.id, rank, value],
                    )?;
                }
            }
        }
    }
    Ok(affected)
}

pub(super) fn replace_desired(
    tx: &Transaction<'_>,
    namespace: &Namespace,
    agent: &str,
    selected: Option<(&DesiredSubject, &str, &str)>,
) -> Result<()> {
    anyhow::ensure!(
        agent.starts_with("agent/"),
        "invalid selected declaration subject"
    );
    tx.execute(
        "DELETE FROM local_agent_card_owner_dependents WHERE namespace=?1 AND agent=?2 AND owner NOT LIKE '#operation:%'",
        params![namespace.as_str(), agent],
    )?;
    tx.execute(
        "INSERT OR IGNORE INTO local_agent_card_base_declarations VALUES(?1,?2)",
        params![namespace.as_str(), agent],
    )?;
    if let Some((desired, token, revision)) = selected {
        anyhow::ensure!(
            desired.subject == agent,
            "selected declaration subject mismatch"
        );
        tx.execute("INSERT INTO local_agent_card_base_desired VALUES(?1,?2,?3,?4,?5) ON CONFLICT(namespace,agent) DO UPDATE SET token=excluded.token,revision=excluded.revision,body=excluded.body",params![namespace.as_str(),agent,token,revision,serde_json::to_string(desired)?])?;
        for owner in desired
            .owner_run
            .iter()
            .chain(desired.owner_generation.iter())
        {
            tx.execute(
                "INSERT OR IGNORE INTO local_agent_card_owner_dependents VALUES(?1,?2,?3)",
                params![namespace.as_str(), owner, agent],
            )?;
        }
    } else {
        tx.execute(
            "DELETE FROM local_agent_card_base_desired WHERE namespace=?1 AND agent=?2",
            params![namespace.as_str(), agent],
        )?;
    }
    Ok(())
}

/// Inclusive indexed fanout pages prevent an owner completion from rebuilding
/// every seat in one source callback. Incomplete fanout keeps coverage pending.
pub(super) fn drain_owners(
    tx: &Transaction<'_>,
    namespace: &Namespace,
    limit: usize,
) -> Result<(usize, BTreeSet<String>)> {
    drain(tx, namespace.as_str(), limit)
}
fn drain(tx: &Transaction<'_>, namespace: &str, limit: usize) -> Result<(usize, BTreeSet<String>)> {
    anyhow::ensure!((1..=128).contains(&limit), "invalid owner fanout page");
    let mut used = 0;
    let mut agents = BTreeSet::new();
    while used < limit {
        let dirty: Option<(String,String)> = tx.query_row("SELECT owner,after FROM local_agent_card_owner_dirty WHERE namespace=?1 ORDER BY owner LIMIT 1",[namespace],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        let Some((owner, after)) = dirty else {
            break;
        };
        let rows = tx.prepare_cached("SELECT agent FROM local_agent_card_owner_dependents WHERE namespace=?1 AND owner=?2 AND agent>?3 ORDER BY agent LIMIT ?4")?.query_map(params![namespace,owner,after,limit-used+1],|r|r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        let take = rows.len().min(limit - used);
        if let Some(operation) = owner.strip_prefix("#operation:") {
            for agent in rows.iter().take(take) {
                operation_agent(tx, namespace, operation, agent)?;
            }
        }
        agents.extend(rows.iter().take(take).cloned());
        used += take.max(1);
        if rows.len() > take {
            tx.execute(
                "UPDATE local_agent_card_owner_dirty SET after=?3 WHERE namespace=?1 AND owner=?2",
                params![namespace, owner, rows[take - 1]],
            )?;
        } else {
            tx.execute(
                "DELETE FROM local_agent_card_owner_dirty WHERE namespace=?1 AND owner=?2",
                params![namespace, owner],
            )?;
        }
    }
    Ok((used, agents))
}

fn owner_field(
    connection: &Connection,
    namespace: &str,
    subject: &str,
    field: &str,
) -> Result<Option<String>> {
    let value: Option<Option<String>> = connection.query_row("SELECT value FROM local_agent_card_owner_fields WHERE namespace=?1 AND subject=?2 AND field=?3 ORDER BY rank DESC,id DESC LIMIT 1",params![namespace,subject,field],|r|r.get(0)).optional()?;
    Ok(value
        .flatten()
        .map(|v| serde_json::from_str::<Value>(&v))
        .transpose()?
        .and_then(|v| v.as_str().map(str::to_owned)))
}

fn annotation(
    connection: &Connection,
    namespace: &str,
    agent: &str,
    desired: Option<&DesiredSubject>,
    actual: Option<&Value>,
) -> Result<OperationalAnnotation> {
    let fields = actual.map(|a| a.get("fields").unwrap_or(a));
    let status = fields.and_then(|f| f["status"].as_str());
    let incarnation = fields
        .and_then(|f| f["incarnation_id"].as_str())
        .map(str::to_owned);
    let mut reasons = Vec::new();
    if matches!(status, Some("stopped" | "absent" | "exited"))
        && desired.is_none_or(|d| d.kind == "stop")
    {
        reasons.push("stopped".into());
    }
    if desired.is_none() && status.is_none() {
        reasons.push("undeclared".into());
    }
    if let Some(desired) = desired
        && let Some(run) = &desired.owner_run
    {
        if owner_field(connection, namespace, run, "status")?
            .as_deref()
            .is_some_and(is_terminal_run_state)
        {
            reasons.push("terminal-owner".into());
            if owner_field(connection, namespace, run, "mode")?.as_deref() == Some("eval") {
                reasons.push("eval".into());
            }
        }
        if let Some(generation) = &desired.owner_generation {
            let state = owner_field(connection, namespace, generation, "status")?;
            if state.as_deref() == Some("superseded") {
                reasons.push("superseded".into());
            } else if state.as_deref().is_some_and(is_terminal_generation_state) {
                reasons.push("terminal-generation".into());
            }
        }
    }
    reasons.sort();
    reasons.dedup();
    let current = reasons.is_empty();
    let healthy = matches!(status, Some("running" | "ready" | "working" | "idle"));
    if current && desired.is_some() && !healthy {
        reasons.push("unhealthy".into());
    }
    let _ = agent; // Agent-only callers; owner selection remains snapshot-index semantics.
    Ok(OperationalAnnotation {
        layer: if current { "current" } else { "history" }.into(),
        actionable: current && healthy,
        reasons,
        owner_generation: desired.and_then(|d| d.owner_generation.clone()),
        runtime_incarnation: incarnation,
    })
}

pub(super) fn clean(connection: &Connection, namespace: &Namespace) -> Result<bool> {
    Ok(!connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM local_agent_card_owner_dirty WHERE namespace=?1)",
        [namespace.as_str()],
        |r| r.get::<_, bool>(0),
    )?)
}

pub(super) fn replace_operation(
    tx: &Transaction<'_>,
    namespace: &Namespace,
    id: &str,
    state: Option<&str>,
) -> Result<()> {
    tx.execute("INSERT INTO local_agent_card_operation_states VALUES(?1,?2,?3) ON CONFLICT(namespace,id) DO UPDATE SET state=excluded.state",params![namespace.as_str(),id,state.unwrap_or("absent")])?;
    dirty_owner(tx, namespace.as_str(), &format!("#operation:{id}"))
}

pub(super) fn selected_desired(
    connection: &Connection,
    namespace: &Namespace,
    agent: &str,
) -> Result<Option<(DesiredSubject, String, String)>> {
    let known: bool = connection.query_row("SELECT EXISTS(SELECT 1 FROM local_agent_card_base_declarations WHERE namespace=?1 AND agent=?2)",params![namespace.as_str(),agent],|r|r.get(0))?;
    anyhow::ensure!(known, "selected declaration not captured");
    let row: Option<(String,String,String)> = connection.query_row("SELECT body,token,revision FROM local_agent_card_base_desired WHERE namespace=?1 AND agent=?2",params![namespace.as_str(),agent],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
    row.map(|(body, token, revision)| Ok((serde_json::from_str(&body)?, token, revision)))
        .transpose()
}

pub(super) fn subject(
    connection: &Connection,
    namespace: &Namespace,
    agent: &str,
) -> Result<SubjectStatus> {
    let ns = namespace.as_str();
    let selected = selected_desired(connection, namespace, agent)?;
    let desired = selected.as_ref().map(|(d, _, _)| d);
    let authority = super::agent_authority_ivm::read_authority(
        connection,
        namespace,
        agent,
        desired
            .and_then(|d| d.member.as_ref())
            .map(|m| m.host.as_str()),
    )?;
    let missing: bool = connection.query_row("SELECT EXISTS(SELECT 1 FROM local_agent_card_base_missing_operations WHERE namespace=?1 AND agent=?2)",params![ns,agent],|r|r.get(0))?;
    anyhow::ensure!(!missing, "agent operation state not captured");
    let conflict: Option<String> = connection.query_row("SELECT operation FROM local_agent_card_base_conflicts WHERE namespace=?1 AND agent=?2 ORDER BY operation LIMIT 1",params![ns,agent],|r|r.get(0)).optional()?;
    let unknown: Option<String> = connection.query_row("SELECT kind FROM local_agent_card_base_claims WHERE namespace=?1 AND subject=?2 AND unknown=1 ORDER BY kind,id LIMIT 1",params![ns,agent],|r|r.get(0)).optional()?;
    let unknown = conflict
        .map(|id| format!("idempotency-conflict:{id}"))
        .or(unknown);
    let reachability = if unknown.is_some() || authority.runtime_origin_conflict {
        "indeterminate".into()
    } else {
        authority.reachability.clone().unwrap_or_else(|| {
            if authority.actual_presence {
                "reachable"
            } else if desired.is_some() {
                "indeterminate"
            } else {
                "unknown"
            }
            .into()
        })
    };
    let reason = if authority.runtime_origin_conflict {
        Some("concurrent runtime observations have indeterminate authority".into())
    } else {
        unknown
            .map(|kind| format!("claim kind `{kind}` is not registered"))
            .or(authority.reason.clone())
    };
    let claims: Vec<String> = if desired.is_some() {
        vec![]
    } else {
        connection.query_row("SELECT id FROM local_agent_card_base_claims WHERE namespace=?1 AND subject=?2 ORDER BY rank DESC,id DESC LIMIT 1",params![ns,agent],|r|r.get::<_,String>(0)).optional()?.into_iter().collect()
    };
    let projection = annotation(connection, ns, agent, desired, authority.actual.as_ref())?;
    let body = desired.map(|d| d.desired.clone());
    let under = body
        .as_ref()
        .map(crate::graph::agent_under)
        .unwrap_or_default();
    Ok(SubjectStatus {
        subject: agent.into(),
        kind: desired.map(|d| d.kind.clone()),
        desired_token: selected.as_ref().map(|(_, t, _)| t.clone()),
        desired_revision: selected.as_ref().map(|(_, _, r)| r.clone()),
        desired: body,
        actual: authority.actual,
        actual_claim: authority.actual_claim,
        actual_origin: authority.actual_origin,
        harness: super::agent_card_harness::read_harness(connection, namespace, agent)?,
        conflicts: vec![],
        claims,
        owner_run: desired.and_then(|d| d.owner_run.clone()),
        gap: None,
        reachability,
        reason,
        under,
        projection,
    })
}

pub(super) fn actual_at(
    connection: &Connection,
    namespace: &Namespace,
    claim: &str,
) -> Result<u128> {
    let at: String = connection.query_row(
        "SELECT at FROM local_agent_card_base_claims WHERE namespace=?1 AND id=?2",
        params![namespace.as_str(), claim],
        |r| r.get(0),
    )?;
    Ok(at.parse()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn append(store: &Store, subject: &str, kind: &str, fields: Value, at: u128) -> ClaimRecord {
        store.set_write_clock_at(at).unwrap();
        let mut writer = store.connection.write();
        let tx = writer.transaction().unwrap();
        let claim = smallclaims::store::append_claim_record_tx(
            &tx,
            store.origin(),
            subject,
            kind,
            None,
            &json!({"fields":fields}),
            &[],
            None,
        )
        .unwrap();
        let key = canonical::claim_key(&tx, &claim.id).unwrap();
        apply(&tx, "live", None, Some((&claim, &key))).unwrap();
        tx.commit().unwrap();
        claim
    }
    fn desired() -> DesiredSubject {
        let source = "version 2\nagent \"amber\" { command \"true\" }\n";
        let mut d = crate::graph::parse_test_intent(source, "node")
            .unwrap()
            .subjects
            .remove("agent/node.amber")
            .unwrap();
        d.owner_run = Some("mission-run/invented".into());
        d.owner_generation = Some("run-generation/invented".into());
        d
    }
    fn check(store: &Store, desired: Option<&DesiredSubject>, actual: Option<&Value>) {
        let connection = store.readers.get();
        let index = current_index(&connection).unwrap();
        let expected = operational_annotation(
            &connection,
            "agent/node.amber",
            desired.map(|d| d.kind.as_str()),
            desired.is_some(),
            desired.and_then(|d| d.owner_run.as_deref()),
            desired.and_then(|d| d.owner_generation.as_deref()),
            actual,
            Some(index),
        )
        .unwrap();
        let result = annotation(&connection, "live", "agent/node.amber", desired, actual).unwrap();
        assert_eq!(
            serde_json::to_value(result).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
    }

    #[test]
    fn owner_canonical_sparse_reset_and_late_retraction_match_snapshot_oracle() {
        let store = Store::open_memory("node").unwrap();
        create_schema(&store.connection.write()).unwrap();
        let d = desired();
        let actual = json!({"status":"running","incarnation_id":"one"});
        append(
            &store,
            "mission-run/invented",
            "mission-run.started",
            json!({"status":"running","mode":"eval"}),
            100,
        );
        check(&store, Some(&d), Some(&actual));
        let terminal = append(
            &store,
            "mission-run/invented",
            "mission-run.state",
            json!({"status":"failed","mode":"eval"}),
            300,
        );
        check(&store, Some(&d), Some(&actual));
        append(
            &store,
            "mission-run/invented",
            "mission-run.state",
            json!({"status":"running"}),
            200,
        );
        check(&store, Some(&d), Some(&actual));
        append(
            &store,
            "run-generation/invented",
            "run-generation.state",
            json!({"status":"superseded"}),
            400,
        );
        check(&store, Some(&d), Some(&actual));
        append(
            &store,
            "run-generation/invented",
            "run-generation.state",
            json!({"status":null}),
            500,
        );
        check(&store, Some(&d), Some(&actual));
        let mut writer = store.connection.write();
        let tx = writer.transaction().unwrap();
        tx.execute("DELETE FROM claims WHERE id=?1", [&terminal.id])
            .unwrap();
        apply(&tx, "live", Some(&terminal), None).unwrap();
        tx.commit().unwrap();
        drop(writer);
        check(&store, Some(&d), Some(&actual));
        check(&store, None, None);
        check(&store, None, Some(&json!({"status":"stopped"})));
    }

    #[test]
    fn owner_reverse_fanout_is_bounded_resumable_and_rollback_isolated() {
        let store = Store::open_memory("node").unwrap();
        let mut writer = store.connection.write();
        create_schema(&writer).unwrap();
        let tx = writer.transaction().unwrap();
        for i in 0..201 {
            tx.execute("INSERT INTO local_agent_card_owner_dependents VALUES('staging','mission-run/invented',?1)",[format!("agent/invented/{i:03}")]).unwrap();
        }
        tx.execute("INSERT INTO local_agent_card_owner_dependents VALUES('live','mission-run/invented','agent/published')",[]).unwrap();
        dirty_owner(&tx, "staging", "mission-run/invented").unwrap();
        let (used, first) = drain(&tx, "staging", 7).unwrap();
        assert_eq!(used, 7);
        assert_eq!(first.len(), 7);
        tx.commit().unwrap();
        let tx = writer.transaction().unwrap();
        let (_, second) = drain(&tx, "staging", 7).unwrap();
        assert!(first.is_disjoint(&second));
        tx.rollback().unwrap();
        let tx = writer.transaction().unwrap();
        let mut all = first;
        loop {
            let (used, rows) = drain(&tx, "staging", 7).unwrap();
            if used == 0 {
                break;
            }
            assert!(all.is_disjoint(&rows));
            all.extend(rows);
        }
        assert_eq!(all.len(), 201);
        assert!(!all.contains("agent/published"));
        tx.commit().unwrap();
    }

    #[test]
    fn unknown_operation_state_and_conflict_heads_do_not_walk_claim_history() {
        let store = Store::open_memory("node").unwrap();
        let mut writer = store.connection.write();
        create_schema(&writer).unwrap();
        let tx = writer.transaction().unwrap();
        tx.execute("INSERT INTO local_agent_card_owner_dependents VALUES('n','#operation:invented','agent/invented')",[]).unwrap();
        operation_agent(&tx, "n", "invented", "agent/invented").unwrap();
        let missing = || {
            tx.query_row("SELECT EXISTS(SELECT 1 FROM local_agent_card_base_missing_operations WHERE namespace='n' AND agent='agent/invented')",[],|r|r.get::<_,bool>(0)).unwrap()
        };
        assert!(missing());
        tx.execute(
            "INSERT INTO local_agent_card_operation_states VALUES('n','invented','conflict')",
            [],
        )
        .unwrap();
        dirty_owner(&tx, "n", "#operation:invented").unwrap();
        drain(&tx, "n", 1).unwrap();
        assert!(!missing());
        let conflict: String = tx.query_row("SELECT operation FROM local_agent_card_base_conflicts WHERE namespace='n' AND agent='agent/invented' ORDER BY operation LIMIT 1",[],|r|r.get(0)).unwrap();
        assert_eq!(conflict, "invented");
        tx.execute("UPDATE local_agent_card_operation_states SET state='absent' WHERE namespace='n' AND id='invented'",[]).unwrap();
        dirty_owner(&tx, "n", "#operation:invented").unwrap();
        drain(&tx, "n", 1).unwrap();
        assert!(!missing());
        assert_eq!(
            tx.query_row(
                "SELECT count(*) FROM local_agent_card_base_conflicts WHERE namespace='n'",
                [],
                |r| r.get::<_, u64>(0)
            )
            .unwrap(),
            0
        );
        tx.rollback().unwrap();
    }
}
