//! Namespaced storage and bounded reads for the complete public agent card.
//!
//! The source adapter owns all card calculations and publication. These helpers
//! do not certify a partial kernel, install a registry, or grant session authority.
//! Capture the Installer root, Views boundary, coverage and rows in one authorized
//! Store snapshot. An overdue deadline refuses reads even before a timer commits.

use super::*;
use smallclaims::ivm::install::{Namespace, Root};
use smallclaims::ivm::{Definition, View};

pub(crate) const VIEW: &str = "st3.agents.cards.v1";
pub(crate) const SOURCE: &str = "st3.agent-card-source.v1";
pub(crate) const FINGERPRINT: &str = super::agent_card_source::FINGERPRINT;
pub(crate) const WINDOW_LIMIT: usize = 200;

struct CardView;
impl View for CardView {
    fn definition(&self) -> Definition {
        Definition {
            name: VIEW,
            fingerprint: FINGERPRINT,
            kinds: &[],
            local_kinds: &[],
            max_contributions: 1,
        }
    }
    fn installed_source(&self) -> Option<&'static str> {
        Some(SOURCE)
    }
    fn create_schema(&self, connection: &Connection) -> Result<()> {
        create_schema(connection)
    }
}
pub(crate) fn definitions() -> Vec<Box<dyn View>> {
    vec![Box::new(CardView)]
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS local_agent_card_rows (
 namespace TEXT NOT NULL, agent TEXT NOT NULL,
 name TEXT NOT NULL COLLATE BINARY, state TEXT NOT NULL,
 current INTEGER NOT NULL CHECK(current IN (0,1)),
 key_generation INTEGER NOT NULL CHECK(key_generation>0), body TEXT, body_hash TEXT NOT NULL,
 request_time INTEGER NOT NULL CHECK(request_time IN(0,1)),
 number_bits TEXT NOT NULL,
 PRIMARY KEY(namespace,agent)
);
CREATE INDEX IF NOT EXISTS local_agent_card_current_order
 ON local_agent_card_rows(namespace,current,name,agent) WHERE body IS NOT NULL;
CREATE INDEX IF NOT EXISTS local_agent_card_current_status_order
 ON local_agent_card_rows(namespace,current,state,name,agent) WHERE body IS NOT NULL;
CREATE INDEX IF NOT EXISTS local_agent_card_history_order
 ON local_agent_card_rows(namespace,name,agent) WHERE body IS NOT NULL;
CREATE INDEX IF NOT EXISTS local_agent_card_history_status_order
 ON local_agent_card_rows(namespace,state,name,agent) WHERE body IS NOT NULL;
CREATE TABLE IF NOT EXISTS local_agent_card_coverage (
 namespace TEXT PRIMARY KEY,
 incomplete INTEGER NOT NULL CHECK(incomplete>=0),
 pending INTEGER NOT NULL CHECK(pending>=0),
 source_epoch INTEGER NOT NULL, source_revision INTEGER NOT NULL,
 graph_epoch INTEGER NOT NULL, projected INTEGER NOT NULL, local_generation INTEGER NOT NULL,
 evaluation_time TEXT NOT NULL, next_deadline TEXT
);
"#;

pub(crate) fn create_schema(connection: &Connection) -> Result<()> {
    connection.execute_batch(SCHEMA)?;
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RankedKey {
    pub agent: String,
    pub generation: u64,
    pub body_hash: String,
    pub request_time: bool,
}

#[derive(Debug)]
pub(crate) struct Window {
    pub keys: Vec<RankedKey>,
    pub has_more: bool,
}

/// Coverage is separate from Installer publication and graph Ready. All three
/// must agree; none can restore a raw-gap or missing-input fence in another.
pub(crate) fn coverage(
    connection: &Connection,
    root: &Root,
    cut: &smallclaims::ivm::SourceCut,
    now_unix_ms: u128,
) -> Result<bool> {
    coverage_at(
        connection,
        root.namespace.as_str(),
        root.epoch,
        root.revision,
        cut,
        now_unix_ms,
    )
}

fn coverage_at(
    connection: &Connection,
    namespace: &str,
    source_epoch: u64,
    source_revision: u64,
    cut: &smallclaims::ivm::SourceCut,
    now_unix_ms: u128,
) -> Result<bool> {
    let evidence: Option<(u64, u64, u64, u64, u64, u64, u64, String, Option<String>)> =
        connection
            .query_row(
                "SELECT incomplete,pending,source_epoch,source_revision,graph_epoch,projected,local_generation,evaluation_time,next_deadline FROM local_agent_card_coverage WHERE namespace=?1",
                [namespace],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?)),
            )
            .optional()?;
    let Some((
        incomplete,
        pending,
        epoch,
        revision,
        graph_epoch,
        projected,
        local_generation,
        at,
        deadline,
    )) = evidence
    else {
        return Ok(false);
    };
    let at: u128 = at.parse()?;
    let deadline = deadline.map(|value| value.parse::<u128>()).transpose()?;
    Ok(incomplete == 0
        && pending == 0
        && epoch == source_epoch
        && revision == source_revision
        && graph_epoch == cut.epoch
        && projected == cut.projected
        && cut.projected == cut.admitted
        && local_generation == cut.local_generation
        && now_unix_ms >= at
        && deadline.is_none_or(|deadline| now_unix_ms < deadline))
}

/// This accepts only a certified namespace captured by the caller. It selects
/// public IDs without decoding unrelated rows or consulting any claim history.
pub(crate) fn ranked_window(
    connection: &Connection,
    namespace: &Namespace,
    limit: usize,
    history: bool,
    state: Option<&str>,
) -> Result<Window> {
    window(connection, namespace.as_str(), limit, history, state)
}

fn window(
    connection: &Connection,
    namespace: &str,
    limit: usize,
    history: bool,
    state: Option<&str>,
) -> Result<Window> {
    anyhow::ensure!(
        (1..=WINDOW_LIMIT).contains(&limit),
        "invalid agent window limit"
    );
    let sql = match (history, state.is_some()) {
        (false, false) => {
            "SELECT agent,key_generation,body_hash,request_time FROM local_agent_card_rows WHERE namespace=?1 AND current=1 AND body IS NOT NULL ORDER BY name,agent LIMIT ?2"
        }
        (false, true) => {
            "SELECT agent,key_generation,body_hash,request_time FROM local_agent_card_rows WHERE namespace=?1 AND current=1 AND state=?3 AND body IS NOT NULL ORDER BY name,agent LIMIT ?2"
        }
        (true, false) => {
            "SELECT agent,key_generation,body_hash,request_time FROM local_agent_card_rows WHERE namespace=?1 AND body IS NOT NULL ORDER BY name,agent LIMIT ?2"
        }
        (true, true) => {
            "SELECT agent,key_generation,body_hash,request_time FROM local_agent_card_rows WHERE namespace=?1 AND state=?3 AND body IS NOT NULL ORDER BY name,agent LIMIT ?2"
        }
    };
    let mut statement = connection.prepare(sql)?;
    let mut query = match state {
        Some(state) => statement.query(params![namespace, limit + 1, state])?,
        None => statement.query(params![namespace, limit + 1])?,
    };
    let mut keys = Vec::with_capacity(limit + 1);
    while let Some(row) = query.next()? {
        keys.push(RankedKey {
            agent: row.get(0)?,
            generation: row.get(1)?,
            body_hash: row.get(2)?,
            request_time: row.get(3)?,
        });
    }
    let has_more = keys.len() > limit;
    keys.truncate(limit);
    Ok(Window { keys, has_more })
}

/// Generation is checked even when no key invalidation reached this socket.
/// Return None for a removed row; the caller must refresh its ranked window.
pub(crate) fn row(
    connection: &Connection,
    namespace: &Namespace,
    key: &RankedKey,
) -> Result<Option<Value>> {
    read_row(connection, namespace.as_str(), key)
}

/// The current ranked metadata proves membership/order independently of this
/// comparison. Retained public JSON may be reused only after current authority
/// and complete source coverage have been checked in that same snapshot.
pub(crate) fn reusable(key: &RankedKey, retained: &Value) -> Result<bool> {
    let mut template = retained.clone();
    if key.request_time {
        template["updated_at"] = json!("");
    }
    Ok(retained["id"].as_str() == Some(key.agent.as_str())
        && smallclaims::hash::canonical_hash(&template)? == key.body_hash)
}

/// All authority, Installer binding and coverage checks precede this bounded
/// read in the same snapshot. A blank legacy timestamp is frame presentation;
/// filling it cannot change namespace rows, generations, ordering or status.
pub(crate) fn current_rows(
    connection: &Connection,
    root: &Root,
    limit: usize,
    state: Option<&str>,
    retained: &BTreeMap<String, Value>,
    frame_time: &str,
) -> Result<(Vec<Value>, bool)> {
    current_rows_at(
        connection,
        root.namespace.as_str(),
        limit,
        state,
        retained,
        frame_time,
    )
}
fn current_rows_at(
    connection: &Connection,
    namespace: &str,
    limit: usize,
    state: Option<&str>,
    retained: &BTreeMap<String, Value>,
    frame_time: &str,
) -> Result<(Vec<Value>, bool)> {
    let window = window(connection, namespace, limit, false, state)?;
    let mut values = Vec::with_capacity(window.keys.len());
    for key in window.keys {
        let mut value = if let Some(previous) = retained.get(&key.agent)
            && reusable(&key, previous)?
        {
            previous.clone()
        } else {
            read_row(connection, namespace, &key)?
                .context("ranked agent row changed within snapshot")?
        };
        if key.request_time {
            value["updated_at"] = json!(frame_time);
        }
        values.push(value);
    }
    Ok((values, window.has_more))
}

fn read_row(connection: &Connection, namespace: &str, key: &RankedKey) -> Result<Option<Value>> {
    let body: Option<(String,String)> = connection.query_row(
        "SELECT body,number_bits FROM local_agent_card_rows WHERE namespace=?1 AND agent=?2 AND key_generation=?3 AND body IS NOT NULL",
        params![namespace, key.agent, key.generation],
        |r| Ok((r.get(0)?,r.get(1)?)),
    ).optional()?;
    body.map(|(body, bits)| {
        let mut value: Value = serde_json::from_str(&body)?;
        let bits: BTreeMap<String, u64> = serde_json::from_str(&bits)?;
        for (pointer, bits) in bits {
            let number = f64::from_bits(bits);
            anyhow::ensure!(number.is_finite(), "non-finite public card number");
            *value
                .pointer_mut(&pointer)
                .context("public card numeric pointer missing")? = json!(number);
        }
        Ok(value)
    })
    .transpose()
}

fn numeric_bits(value: &Value) -> Result<BTreeMap<String, u64>> {
    fn visit(
        value: &Value,
        path: &str,
        depth: usize,
        out: &mut BTreeMap<String, u64>,
    ) -> Result<()> {
        anyhow::ensure!(depth <= 128, "public card JSON depth exceeded");
        match value {
            Value::Number(number) if number.is_f64() => {
                out.insert(
                    path.into(),
                    number
                        .as_f64()
                        .context("public card number invalid")?
                        .to_bits(),
                );
            }
            Value::Array(values) => {
                for (index, value) in values.iter().enumerate() {
                    visit(value, &format!("{path}/{index}"), depth + 1, out)?;
                }
            }
            Value::Object(values) => {
                for (name, value) in values {
                    let name = name.replace('~', "~0").replace('/', "~1");
                    visit(value, &format!("{path}/{name}"), depth + 1, out)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    let mut bits = BTreeMap::new();
    visit(value, "", 0, &mut bits)?;
    Ok(bits)
}

/// Called after every dependency family has supplied its current output. This
/// only maintains row equality and generations; it cannot establish coverage.
pub(crate) fn write_row(
    tx: &Transaction<'_>,
    namespace: &Namespace,
    agent: &str,
    current: bool,
    body: Option<&Value>,
) -> Result<bool> {
    replace(tx, namespace.as_str(), agent, current, body)
}

fn replace(
    tx: &Transaction<'_>,
    namespace: &str,
    agent: &str,
    current: bool,
    body: Option<&Value>,
) -> Result<bool> {
    anyhow::ensure!(agent.starts_with("agent/"), "invalid agent public ID");
    let body_hash = body
        .map(smallclaims::hash::canonical_hash)
        .transpose()?
        .unwrap_or_default();
    let request_time = body.is_some_and(|value| value["updated_at"] == "");
    let number_bits =
        serde_json::to_string(&body.map(numeric_bits).transpose()?.unwrap_or_default())?;
    let (name, state, body) = match body {
        None => (String::new(), String::new(), None),
        Some(body) => {
            anyhow::ensure!(
                body["id"].as_str() == Some(agent) && body["kind"] == "agent",
                "agent row identity mismatch"
            );
            let object = body.as_object().context("agent card is not an object")?;
            for field in PUBLIC_FIELDS {
                anyhow::ensure!(object.contains_key(*field), "agent card missing {field}");
            }
            anyhow::ensure!(
                !object.contains_key("_status_source"),
                "agent row contains internal status source"
            );
            let name = body["name"]
                .as_str()
                .context("agent card name is not text")?
                .to_owned();
            let state = body["state"]
                .as_str()
                .context("agent card state is not text")?
                .to_owned();
            (name, state, Some(serde_json::to_string(body)?))
        }
    };
    let previous: Option<(String, String, bool, Option<String>)> = tx.query_row(
        "SELECT name,state,current,body FROM local_agent_card_rows WHERE namespace=?1 AND agent=?2",
        params![namespace, agent], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    ).optional()?;
    let next = (name, state, current, body);
    if previous.as_ref() == Some(&next) || (previous.is_none() && next.3.is_none()) {
        return Ok(false);
    }
    tx.execute(
        "INSERT INTO local_agent_card_rows(namespace,agent,name,state,current,key_generation,body,body_hash,request_time,number_bits) VALUES(?1,?2,?3,?4,?5,1,?6,?7,?8,?9) ON CONFLICT(namespace,agent) DO UPDATE SET name=excluded.name,state=excluded.state,current=excluded.current,key_generation=key_generation+1,body=excluded.body,body_hash=excluded.body_hash,request_time=excluded.request_time,number_bits=excluded.number_bits",
        params![namespace, agent, next.0, next.1, next.2, next.3, body_hash, request_time, number_bits],
    )?;
    Ok(true)
}

const PUBLIC_FIELDS: &[&str] = &[
    "id",
    "kind",
    "revision",
    "updated_at",
    "name",
    "state",
    "reachability",
    "runtime_ids",
    "owner_run_id",
    "driver",
    "harness_state",
    "harness_error_state",
    "since",
    "blocked_on",
    "ask",
    "reason",
    "host_id",
    "workspace",
    "checkout",
    "last_activity_at",
    "silent_since",
    "fault",
    "incarnation_id",
    "current_session_id",
    "current_work_ids",
    "active_work_count",
    "next_work_id",
    "upcoming_work_ids",
    "queued_work_count",
    "current_work",
    "next_work",
    "upcoming_work",
    "usage",
    "under",
    "operational",
    "suspension",
    "handoff",
    "rollout",
    "todo",
    "observation",
    "subagents",
];

#[cfg(test)]
mod tests {
    use super::*;

    fn card(id: &str, name: &str, state: &str) -> Value {
        let mut body = serde_json::Map::new();
        for field in PUBLIC_FIELDS {
            body.insert((*field).into(), Value::Null);
        }
        body.insert("id".into(), json!(id));
        body.insert("kind".into(), json!("agent"));
        body.insert("name".into(), json!(name));
        body.insert("state".into(), json!(state));
        Value::Object(body)
    }

    #[test]
    fn namespace_window_promotions_filters_and_retained_generations() {
        let store = Store::open_memory("grove").unwrap();
        let mut connection = store.connection.write();
        create_schema(&connection).unwrap();
        let tx = connection.transaction().unwrap();
        for (namespace, id, name, state, current) in [
            ("old", "agent/a", "A", "running", true),
            ("new", "agent/a", "Zulu", "waiting", true),
            ("new", "agent/b", "Alpha", "running", true),
            ("new", "agent/c", "Alpha", "running", true),
            ("new", "agent/h", "Before", "running", false),
        ] {
            replace(&tx, namespace, id, current, Some(&card(id, name, state))).unwrap();
        }
        tx.commit().unwrap();
        let first = window(&connection, "new", 1, false, Some("running")).unwrap();
        assert_eq!(first.keys[0].agent, "agent/b");
        assert!(first.has_more);
        assert_eq!(
            window(&connection, "new", 200, true, None)
                .unwrap()
                .keys
                .len(),
            4
        );
        assert_eq!(
            window(&connection, "old", 200, false, None)
                .unwrap()
                .keys
                .len(),
            1
        );
        let old_key = first.keys[0].clone();
        let tx = connection.transaction().unwrap();
        assert!(
            !replace(
                &tx,
                "new",
                "agent/b",
                true,
                Some(&card("agent/b", "Alpha", "running"))
            )
            .unwrap()
        );
        assert!(replace(&tx, "new", "agent/b", false, None).unwrap());
        tx.commit().unwrap();
        assert!(read_row(&connection, "new", &old_key).unwrap().is_none());
        let promoted = window(&connection, "new", 1, false, Some("running")).unwrap();
        assert_eq!(promoted.keys[0].agent, "agent/c");
        let tx = connection.transaction().unwrap();
        replace(
            &tx,
            "new",
            "agent/b",
            true,
            Some(&card("agent/b", "Alpha", "running")),
        )
        .unwrap();
        tx.commit().unwrap();
        let recreated = window(&connection, "new", 1, false, None).unwrap();
        assert_eq!(recreated.keys[0].generation, old_key.generation + 2);
        assert!(read_row(&connection, "new", &old_key).unwrap().is_none());
    }

    #[test]
    fn partial_rows_identity_mismatch_and_rollback_cannot_change_output() {
        let store = Store::open_memory("grove").unwrap();
        let mut connection = store.connection.write();
        create_schema(&connection).unwrap();
        let tx = connection.transaction().unwrap();
        assert!(
            replace(
                &tx,
                "n",
                "agent/a",
                true,
                Some(&json!({"id":"agent/a","kind":"agent"}))
            )
            .is_err()
        );
        assert!(
            replace(
                &tx,
                "n",
                "agent/a",
                true,
                Some(&card("agent/b", "A", "running"))
            )
            .is_err()
        );
        replace(
            &tx,
            "n",
            "agent/a",
            true,
            Some(&card("agent/a", "A", "running")),
        )
        .unwrap();
        tx.rollback().unwrap();
        assert!(
            window(&connection, "n", 200, false, None)
                .unwrap()
                .keys
                .is_empty()
        );
        assert!(window(&connection, "n", 0, false, None).is_err());
        assert!(window(&connection, "n", 201, false, None).is_err());
    }

    #[test]
    fn retained_rows_require_current_content_and_blank_timestamp_is_frame_only() {
        let store = Store::open_memory("grove").unwrap();
        let mut connection = store.connection.write();
        create_schema(&connection).unwrap();
        let mut a = card("agent/a", "A", "running");
        a["updated_at"] = json!("");
        let b = card("agent/b", "B", "waiting");
        let tx = connection.transaction().unwrap();
        replace(&tx, "n", "agent/a", true, Some(&a)).unwrap();
        replace(&tx, "n", "agent/b", true, Some(&b)).unwrap();
        tx.commit().unwrap();
        let (first, more) =
            current_rows_at(&connection, "n", 1, None, &BTreeMap::new(), "frame-one").unwrap();
        assert!(more);
        assert_eq!(first[0]["updated_at"], "frame-one");
        let generation = window(&connection, "n", 1, false, None).unwrap().keys[0].generation;
        let retained = [("agent/a".into(), first[0].clone())].into_iter().collect();
        let (next, _) = current_rows_at(&connection, "n", 1, None, &retained, "frame-two").unwrap();
        assert_eq!(next[0]["updated_at"], "frame-two");
        assert_eq!(
            window(&connection, "n", 1, false, None).unwrap().keys[0].generation,
            generation
        );
        let mut stale = next[0].clone();
        stale["ask"] = json!("obsolete unauthorized prompt");
        let retained = [("agent/a".into(), stale)].into_iter().collect();
        let (fresh, _) =
            current_rows_at(&connection, "n", 1, None, &retained, "frame-three").unwrap();
        assert_eq!(fresh[0]["ask"], a["ask"]);
        let tx = connection.transaction().unwrap();
        replace(&tx, "n", "agent/a", true, None).unwrap();
        tx.commit().unwrap();
        let (promoted, _) =
            current_rows_at(&connection, "n", 1, None, &retained, "frame-four").unwrap();
        assert_eq!(promoted[0]["id"], "agent/b");
        assert!(
            current_rows_at(&connection, "missing", 1, None, &retained, "frame-five")
                .unwrap()
                .0
                .is_empty()
        );
    }

    #[test]
    fn public_row_storage_preserves_float_bits_and_json_pointer_escaping() {
        let store = Store::open_memory("grove").unwrap();
        let mut writer = store.connection.write();
        create_schema(&writer).unwrap();
        let mut value = card("agent/a", "A", "running");
        value["usage"] = json!({"cost":0.9999999999999999,"context":{"used_percent":-0.0},"escaped/~key":[0.9999999999999999]});
        let tx = writer.transaction().unwrap();
        replace(&tx, "n", "agent/a", true, Some(&value)).unwrap();
        tx.commit().unwrap();
        let key = window(&writer, "n", 1, false, None).unwrap().keys.remove(0);
        let actual = read_row(&writer, "n", &key).unwrap().unwrap();
        assert_eq!(actual, value);
        assert_eq!(
            actual["usage"]["context"]["used_percent"]
                .as_f64()
                .unwrap()
                .to_bits(),
            (-0.0_f64).to_bits()
        );
        assert!(reusable(&key, &actual).unwrap());
        let mut altered = actual;
        altered["usage"]["cost"] = json!(1.0);
        assert!(!reusable(&key, &altered).unwrap());
    }

    #[test]
    fn independent_coverage_rejects_local_changes_deadlines_and_pending_inputs() {
        let store = Store::open_memory("grove").unwrap();
        let connection = store.connection.write();
        create_schema(&connection).unwrap();
        let cut = smallclaims::ivm::SourceCut {
            epoch: 3,
            admitted: 8,
            projected: 8,
            local_generation: 4,
        };
        assert!(!coverage_at(&connection, "n", 1, 12, &cut, 100).unwrap());
        connection
            .execute(
                "INSERT INTO local_agent_card_coverage VALUES('n',0,0,1,12,3,8,4,'100','191')",
                [],
            )
            .unwrap();
        assert!(coverage_at(&connection, "n", 1, 12, &cut, 190).unwrap());
        assert!(!coverage_at(&connection, "n", 1, 12, &cut, 191).unwrap());
        assert!(!coverage_at(&connection, "n", 1, 12, &cut, 99).unwrap());
        assert!(!coverage_at(&connection, "n", 1, 13, &cut, 100).unwrap());
        assert!(
            !coverage_at(
                &connection,
                "n",
                1,
                12,
                &smallclaims::ivm::SourceCut {
                    local_generation: 5,
                    ..cut
                },
                100
            )
            .unwrap()
        );
        assert!(
            !coverage_at(
                &connection,
                "n",
                1,
                12,
                &smallclaims::ivm::SourceCut { admitted: 9, ..cut },
                100
            )
            .unwrap()
        );
        connection
            .execute(
                "UPDATE local_agent_card_coverage SET incomplete=1 WHERE namespace='n'",
                [],
            )
            .unwrap();
        assert!(!coverage_at(&connection, "n", 1, 12, &cut, 100).unwrap());
        connection
            .execute(
                "UPDATE local_agent_card_coverage SET incomplete=0,pending=1 WHERE namespace='n'",
                [],
            )
            .unwrap();
        assert!(!coverage_at(&connection, "n", 1, 12, &cut, 100).unwrap());
    }

    #[test]
    fn all_window_shapes_use_ranked_indexes() {
        let store = Store::open_memory("grove").unwrap();
        let connection = store.connection.write();
        create_schema(&connection).unwrap();
        for (query, index) in [
            (
                "SELECT agent,key_generation,body_hash,request_time FROM local_agent_card_rows WHERE namespace='n' AND current=1 AND body IS NOT NULL ORDER BY name,agent LIMIT 201",
                "local_agent_card_current_order",
            ),
            (
                "SELECT agent,key_generation,body_hash,request_time FROM local_agent_card_rows WHERE namespace='n' AND current=1 AND state='running' AND body IS NOT NULL ORDER BY name,agent LIMIT 201",
                "local_agent_card_current_status_order",
            ),
            (
                "SELECT agent,key_generation,body_hash,request_time FROM local_agent_card_rows WHERE namespace='n' AND body IS NOT NULL ORDER BY name,agent LIMIT 201",
                "local_agent_card_history_order",
            ),
            (
                "SELECT agent,key_generation,body_hash,request_time FROM local_agent_card_rows WHERE namespace='n' AND state='running' AND body IS NOT NULL ORDER BY name,agent LIMIT 201",
                "local_agent_card_history_status_order",
            ),
        ] {
            // The text is a fixed test statement, never a source or caller SQL fragment.
            let query = format!("EXPLAIN QUERY PLAN {query}");
            let plan = connection
                .prepare(&query)
                .unwrap()
                .query_map([], |r| r.get::<_, String>(3))
                .unwrap()
                .map(|row| row.unwrap())
                .collect::<Vec<_>>()
                .join(" ");
            assert!(plan.contains(index), "{plan}");
            assert!(!plan.contains("TEMP B-TREE"), "{plan}");
        }
    }
}
