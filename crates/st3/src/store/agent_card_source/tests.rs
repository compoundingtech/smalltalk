use super::*;
use smallclaims::ivm::install::{Installer, Limits, Root, ScanPage};

pub(crate) fn context(store: &Store) -> Namespace {
    struct Context(Arc<std::sync::Mutex<Option<Namespace>>>);
    impl Operator for Context {
        fn name(&self) -> &'static str {
            "fixture.agent-card.context"
        }
        fn fingerprint(&self) -> &'static str {
            "fixture.only.no.publication"
        }
        fn source(&self) -> &'static str {
            "fixture.agent-card.context"
        }
        fn create_schema(&self, c: &Connection) -> Result<()> {
            Kernel::new("node").create_schema(c)
        }
        fn apply(&self, _: &Transaction<'_>, ns: &Namespace, _: &[Mutation]) -> Result<bool> {
            *self.0.lock().unwrap() = Some(ns.clone());
            Ok(false)
        }
        fn validate_publication(&self, _: &Transaction<'_>, _: &Namespace) -> Result<()> {
            anyhow::bail!("fixture cannot certify a public source")
        }
        fn reclaim(&self, _: &Transaction<'_>, _: &Namespace, _: usize) -> Result<bool> {
            Ok(false)
        }
    }
    let slot = Arc::new(std::sync::Mutex::new(None));
    let installer = Installer::new(vec![Box::new(Context(slot.clone()))]).unwrap();
    let mut writer = store.connection.write();
    installer.create_schema(&writer).unwrap();
    collection_ivm::delivery::create_schema(&writer).unwrap();
    agent_source::clock::create_schema(&writer).unwrap();
    let tx = writer.transaction().unwrap();
    if installer
        .position(&tx, "fixture.agent-card.context")
        .is_err()
    {
        installer
            .register_source(&tx, "fixture.agent-card.context", "fixture.no.ready", 1)
            .unwrap();
    }
    let job = installer
        .start(
            &tx,
            "fixture.agent-card.context",
            Limits {
                page_rows: 128,
                page_bytes: 1024 * 1024,
                pending_rows: 4096,
                pending_bytes: 16 * 1024 * 1024,
                total_rows: 100000,
                callback_ms: 1000,
                lifetime_ms: 60000,
            },
            0,
        )
        .unwrap();
    installer
        .scan(
            &tx,
            &ScanPage {
                job: job.clone(),
                expected_cursor: vec![],
                next_cursor: vec![1],
                position: installer
                    .position(&tx, "fixture.agent-card.context")
                    .unwrap(),
                rows: vec![],
                finished: true,
            },
            0,
        )
        .unwrap();
    assert!(installer.root(&tx, "fixture.agent-card.context").is_err());
    tx.commit().unwrap();
    slot.lock().unwrap().clone().unwrap()
}

// Test oracle extraction may scan complete history. Production uses the source owner's
// indexed extractor and native prefix proof; these provisional namespace controls mint no cut.
pub(crate) fn capture(store: &Store) -> Vec<Mutation> {
    use rusqlite::types::ValueRef;
    let c = store.readers.get();
    c.execute_batch("BEGIN DEFERRED").unwrap();
    let mut all = vec![];
    for table in agent_source::TABLES {
        let query = format!(
            "SELECT {} FROM {} ORDER BY {}",
            table.columns.join(","),
            table.name,
            table.key.join(",")
        );
        let mut statement = c.prepare(&query).unwrap();
        let mut rows = statement.query([]).unwrap();
        while let Some(row) = rows.next().unwrap() {
            let mut body = serde_json::Map::new();
            for (i, column) in table.columns.iter().enumerate() {
                let value = match row.get_ref(i).unwrap() {
                    ValueRef::Null => Value::Null,
                    ValueRef::Integer(v) => json!(v),
                    ValueRef::Real(v) => json!(v),
                    ValueRef::Text(v) => json!(std::str::from_utf8(v).unwrap()),
                    ValueRef::Blob(v) => json!({"$blob":hex::encode_upper(v)}),
                };
                body.insert((*column).into(), value);
            }
            let key = table
                .key
                .iter()
                .map(|k| body[*k].clone())
                .collect::<Vec<_>>();
            all.push(Mutation {
                key: physical_key(table.name, json!(key)).unwrap(),
                old: None,
                new: Some(Value::Object(body)),
            });
        }
    }
    c.execute_batch("COMMIT").unwrap();
    all
}

fn append(store: &Store, kind: &str, fields: Value) -> ClaimRecord {
    store
        .append_claim(&ClaimInput {
            subject: "agent/node.amber".into(),
            kind: kind.into(),
            actor: Some("person/fixture".into()),
            fields: serde_json::from_value(fields).unwrap(),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap()
}
fn seed() -> Store {
    let store = Store::open_memory("node").unwrap();
    let desired = crate::graph::parse_test_intent(
        "version 2\nagent \"amber\" { command \"true\" }\n",
        "node",
    )
    .unwrap()
    .subjects
    .remove("agent/node.amber")
    .unwrap();
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    smallclaims::store::append_claim_record_tx(
        &tx,
        "node",
        "agent/node.amber",
        "intent.desired",
        Some("person/fixture"),
        &serde_json::to_value(desired).unwrap(),
        &[],
        None,
    )
    .unwrap();
    tx.commit().unwrap();
    drop(writer);
    append(
        &store,
        "runtime.observed",
        json!({"status":"running","runtime_id":"node.amber","incarnation_id":"one","host":"node"}),
    );
    append(
        &store,
        "harness.observed",
        json!({"incarnation_id":"one","state":"idle"}),
    );
    store
}
pub(crate) fn clock(store: &Store) -> u128 {
    let at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    agent_source::clock::tick(&tx, at, agent_source::clock::Reason::Kernel).unwrap();
    tx.commit().unwrap();
    at
}
pub(crate) fn drain(store: &Store, ns: &Namespace, kernel: &Kernel) {
    for _ in 0..512 {
        let mut writer = store.connection.write();
        let tx = writer.transaction().unwrap();
        kernel.apply(&tx, ns, &[]).unwrap();
        let clean = kernel
            .families_closed(&tx, ns, current_at(&tx, ns).unwrap())
            .unwrap()
            && !tx
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM local_agent_card_source_work WHERE namespace=?1)",
                    [ns.as_str()],
                    |r| r.get::<_, bool>(0),
                )
                .unwrap();
        tx.commit().unwrap();
        if clean {
            return;
        }
    }
    let c = store.readers.get();
    let work: Vec<(String, String)> = c
        .prepare("SELECT kind,key FROM local_agent_card_source_work WHERE namespace=?1")
        .unwrap()
        .query_map([ns.as_str()], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    panic!("fixture card source failed to close: {work:?}")
}
pub(crate) fn rows(store: &Store, ns: &Namespace, at: u128) -> Vec<Value> {
    let c = store.readers.get();
    let root = Root {
        namespace: ns.clone(),
        epoch: 0,
        revision: 0,
        generation: 0,
        status_revision: 0,
    };
    agent_card_ivm::current_rows(
        &c,
        &root,
        200,
        None,
        &BTreeMap::new(),
        &crate::api::client_timestamp(at),
    )
    .unwrap()
    .0
}
fn compare(store: &Store, ns: &Namespace, at: u128) {
    let index = current_index(&store.readers.get()).unwrap();
    let expected =
        crate::api::agent_card_source_oracle(store, index, &crate::api::client_timestamp(at))
            .unwrap();
    assert_eq!(rows(store, ns, at), expected);
}

#[test]
fn complete_card_physical_namespace_matches_public_store_and_unrelated_source_changes_no_rows() {
    let store = seed();
    let ns = context(&store);
    let at = clock(&store);
    let kernel = Kernel::new("node");
    let inputs = capture(&store);
    for page in inputs.chunks(128) {
        let mut w = store.connection.write();
        let tx = w.transaction().unwrap();
        kernel.apply(&tx, &ns, page).unwrap();
        tx.commit().unwrap();
    }
    drain(&store, &ns, &kernel);
    compare(&store, &ns, at);
    let before = rows(&store, &ns, at);
    let generation: u64 = store
        .readers
        .get()
        .query_row(
            "SELECT key_generation FROM local_agent_card_rows WHERE namespace=?1",
            [ns.as_str()],
            |r| r.get(0),
        )
        .unwrap();
    store
        .append_claim(&ClaimInput {
            subject: "daemon/fixture/unrelated".into(),
            kind: "daemon.diagnostic".into(),
            actor: Some("person/fixture".into()),
            fields: BTreeMap::from([
                ("severity".into(), json!("warning")),
                ("code".into(), json!("fixture")),
                ("reason".into(), json!("unrelated")),
            ]),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    for page in capture(&store).chunks(128) {
        let mut w = store.connection.write();
        let tx = w.transaction().unwrap();
        kernel.apply(&tx, &ns, page).unwrap();
        tx.commit().unwrap();
    }
    drain(&store, &ns, &kernel);
    assert_eq!(rows(&store, &ns, at), before);
    assert_eq!(
        store
            .readers
            .get()
            .query_row(
                "SELECT key_generation FROM local_agent_card_rows WHERE namespace=?1",
                [ns.as_str()],
                |r| r.get::<_, u64>(0)
            )
            .unwrap(),
        generation
    );
    assert!(
        Kernel::new("node")
            .validate_publication(&store.connection.write().transaction().unwrap(), &ns)
            .is_err()
    );
}

#[test]
fn complete_card_sparse_human_fields_null_clear_and_future_global_replacement_preserve_cut() {
    let store = seed();
    let ns = context(&store);
    let kernel = Kernel::new("node");
    append(
        &store,
        "harness.observed",
        json!({"incarnation_id":"one","state":"blocked","blocked_on":"human","ask":"permission","reason":"approval"}),
    );
    append(
        &store,
        "harness.observed",
        json!({"incarnation_id":"one","state":"idle","status_transition":false}),
    );
    let at = clock(&store);
    for page in capture(&store).chunks(128) {
        let mut w = store.connection.write();
        let tx = w.transaction().unwrap();
        kernel.apply(&tx, &ns, page).unwrap();
        tx.commit().unwrap();
    }
    drain(&store, &ns, &kernel);
    compare(&store, &ns, at);
    let held = rows(&store, &ns, at);
    assert_eq!(held[0]["ask"], "permission");
    append(
        &store,
        "harness.observed",
        json!({"incarnation_id":"one","state":"idle","blocked_on":null,"ask":null,"reason":null}),
    );
    assert_eq!(rows(&store, &ns, at), held);
    for page in capture(&store).chunks(128) {
        let mut w = store.connection.write();
        let tx = w.transaction().unwrap();
        kernel.apply(&tx, &ns, page).unwrap();
        tx.commit().unwrap();
    }
    drain(&store, &ns, &kernel);
    compare(&store, &ns, at);
    assert_eq!(rows(&store, &ns, at)[0]["ask"], Value::Null);
}
