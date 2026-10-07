//! Rollout operation state as fixed canonical field heads. Each phase is a sparse
//! transformation: latest assignment, sticky true, or absent. These heads reproduce
//! the ordered legacy fold without replaying the operation history on a write/read.
//! Owned receipt selection and launch/manual hold facts are captured dependencies.
use super::*;
use crate::rollout::{Operation, Policy, Selection};
use serde::Deserialize;
use smallclaims::ivm::install::Namespace;

const KINDS: &[&str] = &[
    "runtime.action.requested",
    "runtime.action.succeeded",
    "runtime.action.failed",
    "runtime.action.deadline-reached",
];
const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS local_agent_card_rollout_inputs(
 namespace TEXT NOT NULL,id TEXT NOT NULL,agent TEXT NOT NULL,origin TEXT NOT NULL,
 actor TEXT,rank BLOB NOT NULL,request INTEGER NOT NULL,body TEXT NOT NULL,
 PRIMARY KEY(namespace,id)
);
CREATE INDEX IF NOT EXISTS local_agent_card_rollout_request
 ON local_agent_card_rollout_inputs(namespace,agent,origin,request,rank DESC,id DESC) WHERE request=1;
CREATE TABLE IF NOT EXISTS local_agent_card_rollout_fields(
 namespace TEXT NOT NULL,agent TEXT NOT NULL,operation TEXT NOT NULL,origin TEXT NOT NULL,
 actor TEXT NOT NULL,field TEXT NOT NULL,id TEXT NOT NULL,rank BLOB NOT NULL,value TEXT NOT NULL,
 PRIMARY KEY(namespace,id,field)
);
CREATE INDEX IF NOT EXISTS local_agent_card_rollout_head
 ON local_agent_card_rollout_fields(namespace,agent,operation,origin,actor,field,rank DESC,id DESC);
CREATE TABLE IF NOT EXISTS local_agent_card_rollout_selection(
 namespace TEXT NOT NULL,agent TEXT NOT NULL,body TEXT,PRIMARY KEY(namespace,agent)
);
CREATE TABLE IF NOT EXISTS local_agent_card_rollout_rows(
 namespace TEXT NOT NULL,agent TEXT NOT NULL,body TEXT,PRIMARY KEY(namespace,agent)
);
"#;
pub(super) fn create_schema(connection: &Connection) -> Result<()> {
    connection.execute_batch(SCHEMA)?;
    Ok(())
}

#[derive(Serialize, Deserialize)]
struct Selected {
    set: String,
    receipt: String,
    token: String,
    target: String,
    policy: Policy,
    manual: bool,
    owner: Option<String>,
    hold_manual: bool,
}
pub(super) fn replace_selection(
    tx: &Transaction<'_>,
    ns: &Namespace,
    agent: &str,
    selected: Option<&Selection>,
    actual_origin: Option<&str>,
    hold_manual: bool,
) -> Result<()> {
    let facts = selected.map(|s| Selected {
        set: s.set.clone(),
        receipt: s.receipt.clone(),
        token: s.desired_token.clone(),
        target: s.target.clone(),
        policy: s.policy.clone(),
        manual: s.manual,
        owner: s
            .desired
            .member
            .as_ref()
            .map(|m| m.host.clone())
            .or_else(|| actual_origin.map(str::to_owned)),
        hold_manual,
    });
    replace_selected(tx, ns.as_str(), agent, facts.as_ref())
}
fn replace_selected(
    tx: &Transaction<'_>,
    ns: &str,
    agent: &str,
    selected: Option<&Selected>,
) -> Result<()> {
    tx.execute("INSERT INTO local_agent_card_rollout_selection VALUES(?1,?2,?3) ON CONFLICT(namespace,agent) DO UPDATE SET body=excluded.body",params![ns,agent,selected.map(serde_json::to_string).transpose()?])?;
    Ok(())
}

pub(super) fn apply_claim(
    tx: &Transaction<'_>,
    ns: &Namespace,
    old: Option<&ClaimRecord>,
    new: Option<(&ClaimRecord, &canonical::ClaimKey)>,
) -> Result<BTreeSet<String>> {
    apply(tx, ns.as_str(), old, new)
}
fn apply(
    tx: &Transaction<'_>,
    ns: &str,
    old: Option<&ClaimRecord>,
    new: Option<(&ClaimRecord, &canonical::ClaimKey)>,
) -> Result<BTreeSet<String>> {
    let mut agents = BTreeSet::new();
    for claim in old.into_iter().chain(new.map(|(c, _)| c)) {
        if let Some(agent) = tx
            .query_row(
                "SELECT agent FROM local_agent_card_rollout_inputs WHERE namespace=?1 AND id=?2",
                params![ns, claim.id],
                |r| r.get::<_, String>(0),
            )
            .optional()?
        {
            agents.insert(agent);
        }
        tx.execute(
            "DELETE FROM local_agent_card_rollout_fields WHERE namespace=?1 AND id=?2",
            params![ns, claim.id],
        )?;
        tx.execute(
            "DELETE FROM local_agent_card_rollout_inputs WHERE namespace=?1 AND id=?2",
            params![ns, claim.id],
        )?;
    }
    if let Some((claim, key)) = new
        && claim.subject.starts_with("agent/")
        && KINDS.contains(&claim.kind.as_str())
    {
        agents.insert(claim.subject.clone());
        let rank = canonical::sortable_key(key);
        // The legacy operation selector deliberately requires nested fields.
        let request = claim.kind == "runtime.action.requested"
            && claim.actor.is_some()
            && claim.body.pointer("/fields/action").and_then(Value::as_str) == Some("rollout");
        tx.execute(
            "INSERT INTO local_agent_card_rollout_inputs VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            params![
                ns,
                claim.id,
                claim.subject,
                claim.origin,
                claim.actor,
                rank,
                request,
                serde_json::to_string(&claim.body)?
            ],
        )?;
        if let (Some(operation), Some(actor), Some(phase)) = (
            claim
                .body
                .pointer("/fields/operation")
                .and_then(Value::as_str),
            claim.actor.as_deref(),
            claim
                .body
                .pointer("/fields/operation_status")
                .and_then(Value::as_str),
        ) {
            for (field, value) in phase_fields(claim, phase) {
                tx.execute("INSERT INTO local_agent_card_rollout_fields VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",params![ns,claim.subject,operation,claim.origin,actor,field,claim.id,rank,serde_json::to_string(&value)?])?;
            }
        }
    }
    Ok(agents)
}

fn phase_fields(claim: &ClaimRecord, phase: &str) -> Vec<(&'static str, Value)> {
    if phase == "drain-ack" {
        return vec![("drain_ack", json!(claim.accepted_at_unix_ms.to_string()))];
    }
    let state = claim.body.pointer("/fields/rollout");
    let token = state
        .and_then(|s| s["desired_token"].as_str())
        .map(|s| ("desired_token", json!(s)));
    if phase == "start-attempted" {
        return std::iter::once(("start_attempted", json!(true)))
            .chain(token)
            .collect();
    }
    let mut fields = vec![(
        "phase",
        json!({"phase":if phase == "failed-replacement" {"failed"} else {phase},"at":claim.accepted_at_unix_ms.to_string()}),
    )];
    fields.extend(token);
    if let Some(state) = state {
        for field in [
            "native_session_id",
            "native_account",
            "native_path",
            "replacement_incarnation",
        ] {
            if let Some(value) = state[field].as_str() {
                fields.push((field, json!(value)));
            }
        }
        if state["forced"] == true {
            fields.push(("forced", json!(true)));
        }
        fields.push((
            "blocking",
            json!(
                state["blocking"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
            ),
        ));
        fields.push(("reason", json!(state["reason"].as_str())));
    }
    fields
}

fn calculate(connection: &Connection, ns: &str, agent: &str) -> Result<Option<Value>> {
    let selected: Option<Option<String>> = connection
        .query_row(
            "SELECT body FROM local_agent_card_rollout_selection WHERE namespace=?1 AND agent=?2",
            params![ns, agent],
            |r| r.get(0),
        )
        .optional()?;
    let Some(body) = selected.context("rollout selection not captured")? else {
        return Ok(None);
    };
    let selected: Selected = serde_json::from_str(&body)?;
    let request:Option<(String,String,String,String)> = selected.owner.as_ref().map(|owner|connection.query_row("SELECT id,origin,actor,body FROM local_agent_card_rollout_inputs WHERE namespace=?1 AND agent=?2 AND origin=?3 AND request=1 ORDER BY rank DESC,id DESC LIMIT 1",params![ns,agent,owner],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()).transpose()?.flatten();
    let mut operation = if let Some((id, origin, actor, body)) = request {
        let body: Value = serde_json::from_str(&body)?;
        if let Some(data) = body.pointer("/fields/rollout")
            && let Ok(mut op) = serde_json::from_value::<Operation>(data.clone())
        {
            op.id = id.clone();
            op.requested_by = Some(actor.clone());
            let head = |field: &str| -> Result<Option<Value>> {
                let value:Option<String> = connection.query_row("SELECT value FROM local_agent_card_rollout_fields WHERE namespace=?1 AND agent=?2 AND operation=?3 AND origin=?4 AND actor=?5 AND field=?6 ORDER BY rank DESC,id DESC LIMIT 1",params![ns,agent,id,origin,actor,field],|r|r.get(0)).optional()?;
                value.map(|s| Ok(serde_json::from_str(&s)?)).transpose()
            };
            if let Some(value) = head("phase")? {
                op.phase = value["phase"]
                    .as_str()
                    .context("invalid rollout phase head")?
                    .into();
                op.phase_at_unix_ms = value["at"]
                    .as_str()
                    .context("invalid rollout phase time")?
                    .parse()?;
            }
            if let Some(value) = head("drain_ack")? {
                op.drain_ack = Some(
                    value
                        .as_str()
                        .context("invalid rollout drain time")?
                        .parse()?,
                );
            }
            op.start_attempted |= head("start_attempted")?.is_some();
            op.forced |= head("forced")?.is_some();
            if let Some(value) = head("desired_token")? {
                op.desired_token = value.as_str().context("invalid rollout token head")?.into();
            }
            for (field, out) in [
                ("native_session_id", &mut op.native_session_id),
                ("native_account", &mut op.native_account),
                ("native_path", &mut op.native_path),
                ("replacement_incarnation", &mut op.replacement_incarnation),
            ] {
                if let Some(value) = head(field)? {
                    *out = Some(
                        value
                            .as_str()
                            .context("invalid rollout sparse head")?
                            .into(),
                    );
                }
            }
            if let Some(value) = head("blocking")? {
                op.blocking = serde_json::from_value(value)?;
            }
            if let Some(value) = head("reason")? {
                op.reason = serde_json::from_value(value)?;
            }
            if selected.set == op.set
                && selected.target == op.target
                && (selected.policy != op.publication_policy
                    || selected.manual != op.publication_manual)
                && matches!(op.phase.as_str(), "running" | "retired")
            {
                None
            } else {
                if selected.set != op.set
                    || selected.target != op.target
                    || selected.policy != op.publication_policy
                    || selected.manual != op.publication_manual
                {
                    op.phase = "superseded".into();
                }
                Some(op)
            }
        } else {
            None
        }
    } else {
        None
    };
    if let Some(op) = operation.as_ref()
        && op.phase != "superseded"
    {
        return Ok(Some(serde_json::to_value(op)?));
    }
    if selected.manual && selected.hold_manual {
        return Ok(Some(
            json!({"phase":"pending","mode":"manual","publication":"published","status":"published, rollout pending (manual)","set":selected.set,"receipt":selected.receipt,"desired_token":selected.token}),
        ));
    }
    operation
        .take()
        .map(|op| Ok(serde_json::to_value(op)?))
        .transpose()
}

pub(super) fn publish(tx: &Transaction<'_>, ns: &Namespace, agent: &str) -> Result<bool> {
    publish_at(tx, ns.as_str(), agent)
}
fn publish_at(tx: &Transaction<'_>, ns: &str, agent: &str) -> Result<bool> {
    let body = calculate(tx, ns, agent)?
        .map(|v| serde_json::to_string(&v))
        .transpose()?;
    let old: Option<Option<String>> = tx
        .query_row(
            "SELECT body FROM local_agent_card_rollout_rows WHERE namespace=?1 AND agent=?2",
            params![ns, agent],
            |r| r.get(0),
        )
        .optional()?;
    if old.as_ref() == Some(&body) {
        return Ok(false);
    }
    tx.execute("INSERT INTO local_agent_card_rollout_rows VALUES(?1,?2,?3) ON CONFLICT(namespace,agent) DO UPDATE SET body=excluded.body",params![ns,agent,body])?;
    Ok(true)
}
pub(super) fn read(connection: &Connection, ns: &Namespace, agent: &str) -> Result<Option<Value>> {
    let body: Option<Option<String>> = connection
        .query_row(
            "SELECT body FROM local_agent_card_rollout_rows WHERE namespace=?1 AND agent=?2",
            params![ns.as_str(), agent],
            |r| r.get(0),
        )
        .optional()?;
    body.context("rollout output not captured")?
        .map(|s| Ok(serde_json::from_str(&s)?))
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;
    const AGENT: &str = "agent/node.amber";
    struct Fixture {
        store: Store,
        selected: Selection,
    }
    impl Fixture {
        fn new() -> Self {
            let store = Store::open_memory("node").unwrap();
            let d = crate::graph::parse_test_intent(
                "version 2\nagent \"amber\" { command \"true\" }\n",
                "node",
            )
            .unwrap()
            .subjects
            .remove(AGENT)
            .unwrap();
            let selected = Selection {
                set: "owned-set/invented".into(),
                receipt: "receipt/invented".into(),
                source: owned_sets::Source {
                    repository: "invented/ivm".into(),
                    r#ref: "refs/heads/main".into(),
                    sha: "0".repeat(40),
                    sequence: 1,
                },
                policy: Policy::when_idle(10000, false),
                manual: false,
                desired_token: "desired/invented".into(),
                target: crate::rollout::target(&d).unwrap(),
                desired: d,
                actor: Some("person/invented".into()),
            };
            let mut writer = store.connection.write();
            create_schema(&writer).unwrap();
            let tx = writer.transaction().unwrap();
            replace_selected(
                &tx,
                "live",
                AGENT,
                Some(&Selected {
                    set: selected.set.clone(),
                    receipt: selected.receipt.clone(),
                    token: selected.desired_token.clone(),
                    target: selected.target.clone(),
                    policy: selected.policy.clone(),
                    manual: false,
                    owner: Some("node".into()),
                    hold_manual: false,
                }),
            )
            .unwrap();
            tx.commit().unwrap();
            drop(writer);
            Self { store, selected }
        }
        fn seed(&self) -> Operation {
            Operation {
                id: "overridden".into(),
                set: self.selected.set.clone(),
                receipt: self.selected.receipt.clone(),
                source: self.selected.source.clone(),
                desired_token: self.selected.desired_token.clone(),
                target: self.selected.target.clone(),
                publication_policy: self.selected.policy.clone(),
                publication_manual: false,
                policy: self.selected.policy.clone(),
                old_incarnation: "one".into(),
                old_member: self.selected.desired.member.clone().unwrap(),
                deadline_unix_ms: 10000,
                requested_by: None,
                requested_at_unix_ms: 99,
                phase: "draining".into(),
                phase_at_unix_ms: 99,
                drain_ack: None,
                start_attempted: false,
                allowed_work: BTreeMap::new(),
                native_session_id: Some("seed-session".into()),
                native_path: None,
                native_account: None,
                replacement_incarnation: None,
                forced: false,
                blocking: vec!["initial".into()],
                reason: Some("initial reason".into()),
            }
        }
        fn append(
            &self,
            origin: &str,
            actor: Option<&str>,
            kind: &str,
            fields: Value,
            at: u128,
        ) -> ClaimRecord {
            self.store.set_write_clock_at(at).unwrap();
            let mut writer = self.store.connection.write();
            let tx = writer.transaction().unwrap();
            let mut claim = smallclaims::store::append_claim_record_tx(
                &tx,
                origin,
                AGENT,
                kind,
                actor,
                &json!({"fields":fields}),
                &[],
                None,
            )
            .unwrap();
            // The local append clock is monotonic. Model the immutable older
            // accepted stamp that a replicated late arrival actually carries.
            tx.execute(
                "UPDATE claims SET accepted_at_unix_ms=?2 WHERE id=?1",
                params![claim.id, at.to_string()],
            )
            .unwrap();
            claim.accepted_at_unix_ms = at;
            let key = canonical::claim_key(&tx, &claim.id).unwrap();
            apply(&tx, "live", None, Some((&claim, &key))).unwrap();
            publish_at(&tx, "live", AGENT).unwrap();
            tx.commit().unwrap();
            drop(writer);
            self.check();
            claim
        }
        fn check(&self) {
            let connection = self.store.readers.get();
            let expected = rollouts::operation_for_selection(&connection, AGENT, &self.selected)
                .unwrap()
                .map(|o| serde_json::to_value(o).unwrap());
            assert_eq!(calculate(&connection, "live", AGENT).unwrap(), expected);
        }
    }

    #[test]
    fn sparse_rollout_phase_heads_match_ordered_store_fold_under_late_arrival() {
        let f = Fixture::new();
        let request = f.append(
            "node",
            Some("person/invented"),
            "runtime.action.requested",
            json!({"action":"rollout","rollout":f.seed()}),
            100,
        );
        let phase = |name: &str, state: Value| json!({"operation":request.id,"operation_status":name,"rollout":state});
        f.append("node",Some("person/invented"),"runtime.action.succeeded",phase("verifying",json!({"native_session_id":"selected-session","forced":true,"blocking":["new",42],"reason":"waiting"})),300);
        f.append("node",Some("person/invented"),"runtime.action.succeeded",phase("starting",json!({"native_session_id":null,"native_path":"invented/path","forced":false,"blocking":null,"reason":null})),200);
        f.append(
            "node",
            Some("person/invented"),
            "runtime.action.succeeded",
            phase(
                "drain-ack",
                json!({"desired_token":"ignored","reason":"ignored"}),
            ),
            400,
        );
        f.append(
            "node",
            Some("person/invented"),
            "runtime.action.succeeded",
            phase(
                "start-attempted",
                json!({"desired_token":"new-desired","native_path":"ignored"}),
            ),
            500,
        );
        let failed = f.append(
            "node",
            Some("person/invented"),
            "runtime.action.failed",
            phase("failed-replacement", Value::Null),
            600,
        );
        f.append(
            "foreign",
            Some("person/invented"),
            "runtime.action.succeeded",
            phase("running", json!({"native_session_id":"foreign"})),
            700,
        );
        f.append(
            "node",
            Some("person/other"),
            "runtime.action.succeeded",
            phase("running", Value::Null),
            800,
        );
        let mut writer = f.store.connection.write();
        let tx = writer.transaction().unwrap();
        tx.execute("DELETE FROM claims WHERE id=?1", [&failed.id])
            .unwrap();
        apply(&tx, "live", Some(&failed), None).unwrap();
        publish_at(&tx, "live", AGENT).unwrap();
        tx.commit().unwrap();
        drop(writer);
        f.check();
        let value = calculate(&f.store.readers.get(), "live", AGENT)
            .unwrap()
            .unwrap();
        assert_eq!(value["phase"], "verifying");
        assert_eq!(value["native_session_id"], "selected-session");
        assert_eq!(value["native_path"], "invented/path");
        assert_eq!(value["forced"], true);
        assert_eq!(value["desired_token"], "new-desired");
    }

    #[test]
    fn latest_malformed_owner_request_shadows_older_valid_request_and_namespace_isolated() {
        let f = Fixture::new();
        f.append(
            "node",
            Some("person/invented"),
            "runtime.action.requested",
            json!({"action":"rollout","rollout":f.seed()}),
            100,
        );
        f.append(
            "node",
            None,
            "runtime.action.requested",
            json!({"action":"rollout","rollout":null}),
            200,
        );
        f.append(
            "node",
            Some("person/invented"),
            "runtime.action.requested",
            json!({"action":"rollout","rollout":{"invalid":"operation"}}),
            300,
        );
        assert!(
            calculate(&f.store.readers.get(), "live", AGENT)
                .unwrap()
                .is_none()
        );
        assert!(calculate(&f.store.readers.get(), "missing", AGENT).is_err());
        let mut writer = f.store.connection.write();
        let tx = writer.transaction().unwrap();
        replace_selected(&tx, "staging", AGENT, None).unwrap();
        assert!(calculate(&tx, "staging", AGENT).unwrap().is_none());
        tx.rollback().unwrap();
        assert!(calculate(&writer, "staging", AGENT).is_err());
    }
}
