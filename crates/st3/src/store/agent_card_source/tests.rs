use super::*;
use smallclaims::ivm::install::{Installer, Limits, Root, ScanPage};

pub(crate) fn context(store: &Store) -> Namespace {
    context_for(store, "fixture.agent-card.context")
}

pub(crate) fn context_for(store: &Store, name: &'static str) -> Namespace {
    struct Context {
        slot: Arc<std::sync::Mutex<Option<Namespace>>>,
        name: &'static str,
    }
    impl Operator for Context {
        fn name(&self) -> &'static str {
            self.name
        }
        fn fingerprint(&self) -> &'static str {
            "fixture.only.no.publication"
        }
        fn source(&self) -> &'static str {
            self.name
        }
        fn create_schema(&self, c: &Connection) -> Result<()> {
            Kernel::new("node").create_schema(c)
        }
        fn apply(&self, _: &Transaction<'_>, ns: &Namespace, _: &[Mutation]) -> Result<bool> {
            *self.slot.lock().unwrap() = Some(ns.clone());
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
    let installer = Installer::new(vec![Box::new(Context {
        slot: slot.clone(),
        name,
    })])
    .unwrap();
    let mut writer = store.connection.write();
    installer.create_schema(&writer).unwrap();
    collection_ivm::delivery::create_schema(&writer).unwrap();
    agent_source::clock::create_schema(&writer).unwrap();
    let tx = writer.transaction().unwrap();
    if installer.position(&tx, name).is_err() {
        installer
            .register_source(&tx, name, "fixture.no.ready", 1)
            .unwrap();
    }
    let job = installer
        .start(
            &tx,
            name,
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
                position: installer.position(&tx, name).unwrap(),
                rows: vec![],
                finished: true,
            },
            0,
        )
        .unwrap();
    assert!(installer.root(&tx, name).is_err());
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

#[test]
fn exact_budget_deadline_transfer_drains_staged_absent_cards_before_requeue() {
    let store = seed();
    let ns = context(&store);
    let at = clock(&store);
    let kernel = Kernel::new("node");
    for inputs in capture(&store).chunks(WORK) {
        let mut w = store.connection.write();
        let tx = w.transaction().unwrap();
        kernel.apply(&tx, &ns, inputs).unwrap();
        tx.commit().unwrap();
    }
    drain(&store, &ns, &kernel);
    {
        let mut w = store.connection.write();
        let tx = w.transaction().unwrap();
        // Retained deadlines for subjects removed from the complete captured source.
        // There are exactly WORK due keys, which previously consumed every retry budget.
        for n in 0..WORK {
            tx.execute(
                "INSERT INTO local_agent_card_source_cards VALUES(?1,?2,?3,'[]',NULL,NULL)",
                params![
                    ns.as_str(),
                    format!("agent/expired/{n:03}"),
                    at.to_be_bytes().as_slice()
                ],
            )
            .unwrap();
        }
        tx.commit().unwrap();
    }
    for _ in 0..3 {
        let mut w = store.connection.write();
        let tx = w.transaction().unwrap();
        kernel.apply(&tx, &ns, &[]).unwrap();
        tx.commit().unwrap();
    }
    let c = store.readers.get();
    let remaining: usize=c.query_row("SELECT count(*) FROM local_agent_card_source_cards WHERE namespace=?1 AND agent LIKE 'agent/expired/%'",[ns.as_str()],|r|r.get(0)).unwrap();
    assert_eq!(
        remaining, 0,
        "due retries must reach absent-card materialization"
    );
    assert!(!card_work(&c, &ns).unwrap());
    assert!(kernel.families_closed(&c, &ns, at).unwrap());
}

#[test]
fn unacknowledged_public_queue_updates_fence_footprint_even_when_computation_is_clean() {
    let store = seed();
    let ns = context(&store);
    let at = clock(&store);
    let kernel = Kernel::new("node");
    for inputs in capture(&store).chunks(WORK) {
        let mut w = store.connection.write();
        let tx = w.transaction().unwrap();
        kernel.apply(&tx, &ns, inputs).unwrap();
        tx.commit().unwrap();
    }
    drain(&store, &ns, &kernel);
    let mut w = store.connection.write();
    let tx = w.transaction().unwrap();
    tx.execute(
        "INSERT INTO local_agent_queue_dirty VALUES(?1,'agent/removed')",
        [ns.as_str()],
    )
    .unwrap();
    assert!(kernel.dependencies_closed(&tx, &ns, at).unwrap());
    assert!(!kernel.families_closed(&tx, &ns, at).unwrap());
    assert!(footprint(&tx, &ns, "node").is_err());
    assert!(kernel.validate_publication(&tx, &ns).is_err());
    tx.commit().unwrap();
    drop(w);
    drain(&store, &ns, &kernel);
    let c = store.readers.get();
    assert!(!queue_public_pending(&c, &ns).unwrap());
    assert!(footprint(&c, &ns, "node").is_ok());
}

#[test]
fn pending_work_seek_cursor_reaches_later_keys_and_rollback_restores_continuation() {
    let store = seed();
    let ns = context(&store);
    let kernel = Kernel::new("node");
    let mut w = store.connection.write();
    let tx = w.transaction().unwrap();
    kernel.apply(&tx, &ns, &[]).unwrap();
    for n in 0..257 {
        queue(&tx, &ns, "operation", &format!("pending/{n:03}")).unwrap();
    }
    let first = page(&tx, &ns, "operation", WORK).unwrap();
    assert_eq!(first.first().unwrap(), "pending/000");
    assert_eq!(first.last().unwrap(), "pending/127");
    tx.commit().unwrap();
    {
        let tx = w.transaction().unwrap();
        let second = page(&tx, &ns, "operation", WORK).unwrap();
        assert_eq!(second.first().unwrap(), "pending/128");
        assert_eq!(second.last().unwrap(), "pending/255");
        tx.rollback().unwrap();
    }
    let tx = w.transaction().unwrap();
    let second = page(&tx, &ns, "operation", WORK).unwrap();
    assert_eq!(second.first().unwrap(), "pending/128");
    tx.commit().unwrap();
    let tx = w.transaction().unwrap();
    assert_eq!(page(&tx, &ns, "operation", WORK).unwrap(), ["pending/256"]);
    assert_eq!(
        page(&tx, &ns, "operation", WORK).unwrap().first().unwrap(),
        "pending/000"
    );
    tx.rollback().unwrap();
}

#[test]
fn native_request_classifier_finds_unmaterialized_card_from_normal_store_declaration() {
    let store = seed();
    let source = "version 2\nagent \"amber\" { workspace \"/work\"; harness \"codex\" {} }\n";
    let intent = crate::graph::parse_test_intent(source, "node").unwrap();
    let plan = store
        .mission(
            &intent,
            crate::model::IntentInput {
                kdl: source.into(),
                source_name: None,
            },
        )
        .unwrap();
    store
        .apply(&intent, &plan.subject_tokens, "native-request-control")
        .unwrap();
    append(
        &store,
        "harness.observed",
        json!({"incarnation_id":"one","state":"idle","driver":"codex"}),
    );
    let ns = context(&store);
    let at = clock(&store);
    let kernel = Kernel::new("node");
    for inputs in capture(&store).chunks(WORK) {
        let mut w = store.connection.write();
        let tx = w.transaction().unwrap();
        kernel.apply(&tx, &ns, inputs).unwrap();
        tx.commit().unwrap();
    }
    for _ in 0..32 {
        let mut w = store.connection.write();
        let tx = w.transaction().unwrap();
        kernel.apply(&tx, &ns, &[]).unwrap();
        tx.commit().unwrap();
    }
    let c = store.readers.get();
    assert_eq!(
        producer_requests(&c, &ns, "current-epoch", at.try_into().unwrap(), 128).unwrap(),
        [("agent/node.amber".into(), "codex".into())]
    );
    assert!(
        rows(&store, &ns, at).is_empty(),
        "missing native input must not produce a partial public card"
    );
    assert!(footprint(&c, &ns, "node").is_err());
}

#[test]
fn native_requests_reject_foreign_epoch_expired_and_missing_inputs_with_shared_bound() {
    let store = seed();
    let ns = context(&store);
    let mut w = store.connection.write();
    let tx = w.transaction().unwrap();
    // Private classifier controls only: these values never pass a live producer guard,
    // stamp source coverage, or publish an Installer root.
    let certificate = |recipient: &str, epoch: &str, deadline: u64| {
        serde_json::from_value::<DeliveryCertificate>(json!({"recipient":recipient,"driver":"codex","epoch":epoch,"revision":1,"evaluation_time_ms":10,"next_deadline_ms":deadline,"watermark_ns":1,"deadline_ns":2,"follows":null})).unwrap()
    };
    for n in 0..512 {
        let agent = format!("agent/ready/{n:03}");
        native_binding(
            &tx,
            &ns,
            &agent,
            Some("codex"),
            Some(&certificate(&agent, "current", 100)),
        )
        .unwrap();
    }
    native_binding(&tx, &ns, "agent/missing", Some("codex"), None).unwrap();
    native_binding(
        &tx,
        &ns,
        "agent/old",
        Some("codex"),
        Some(&certificate("agent/old", "before", 100)),
    )
    .unwrap();
    native_binding(
        &tx,
        &ns,
        "agent/replaced",
        Some("codex"),
        Some(&certificate("agent/replaced", "later", 100)),
    )
    .unwrap();
    native_binding(
        &tx,
        &ns,
        "agent/expired",
        Some("codex"),
        Some(&certificate("agent/expired", "current", 20)),
    )
    .unwrap();
    let expected = BTreeSet::from([
        ("agent/missing".into(), "codex".into()),
        ("agent/old".into(), "codex".into()),
        ("agent/replaced".into(), "codex".into()),
        ("agent/expired".into(), "codex".into()),
    ]);
    assert_eq!(
        producer_requests(&tx, &ns, "current", 20, 128)
            .unwrap()
            .into_iter()
            .collect::<BTreeSet<_>>(),
        expected
    );
    assert_eq!(
        producer_requests(&tx, &ns, "current", 20, 1).unwrap(),
        [("agent/missing".into(), "codex".into())]
    );
    assert!(producer_requests(&tx, &ns, "current", 20, 129).is_err());
    assert!(producer_requests(&tx, &ns, "", 20, 128).is_err());
    for agent in [
        "agent/missing",
        "agent/old",
        "agent/replaced",
        "agent/expired",
    ] {
        native_binding(&tx, &ns, agent, None, None).unwrap();
    }
    assert!(
        producer_requests(&tx, &ns, "current", 20, 128)
            .unwrap()
            .is_empty()
    );
    tx.rollback().unwrap();
}

#[test]
fn future_local_activity_anchor_is_excluded_then_promoted_by_captured_claim_position() {
    let store = seed();
    let ns = context(&store);
    let index = store.index().unwrap();
    let observed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    {
        let mut w = store.connection.write();
        let tx = w.transaction().unwrap();
        // A retained local anchor may exceed the current position after a checkpoint trim.
        // This physical fixture exercises that supported read predicate without a runtime.
        tx.execute("INSERT INTO local_observations(after_store_index,subject,kind,body,observed_at_unix_ms) VALUES(?1,'agent/node.amber','harness.timeline',?2,?3)",params![index+1,json!({"fields":{"incarnation_id":"one","entry_type":"message"}}).to_string(),observed]).unwrap();
        tx.commit().unwrap();
    }
    let at = clock(&store);
    let kernel = Kernel::new("node");
    for inputs in capture(&store).chunks(WORK) {
        let mut w = store.connection.write();
        let tx = w.transaction().unwrap();
        kernel.apply(&tx, &ns, inputs).unwrap();
        tx.commit().unwrap();
    }
    drain(&store, &ns, &kernel);
    compare(&store, &ns, at);
    assert_eq!(rows(&store, &ns, at)[0]["last_activity_at"], Value::Null);
    store
        .append_claim(&ClaimInput {
            subject: "daemon/fixture".into(),
            kind: "daemon.diagnostic".into(),
            actor: None,
            fields: BTreeMap::from([
                ("severity".into(), json!("warning")),
                ("code".into(), json!("fixture")),
                ("reason".into(), json!("advance admission")),
            ]),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    assert_eq!(store.index().unwrap(), index + 1);
    let at = clock(&store);
    for inputs in capture(&store).chunks(WORK) {
        let mut w = store.connection.write();
        let tx = w.transaction().unwrap();
        kernel.apply(&tx, &ns, inputs).unwrap();
        tx.commit().unwrap();
    }
    drain(&store, &ns, &kernel);
    compare(&store, &ns, at);
    assert_eq!(
        rows(&store, &ns, at)[0]["last_activity_at"],
        crate::api::client_timestamp(u128::from(observed))
    );
}

#[test]
fn equal_anchor_repair_pages_preserve_seek_continuation_and_current_head_skips_future_rows() {
    let store = seed();
    let ns = context(&store);
    let mut w = store.connection.write();
    let tx = w.transaction().unwrap();
    agent_card_signals::set_captured_cut(&tx, &ns, 10).unwrap();
    for n in 0..4096 {
        tx.execute("INSERT INTO local_agent_card_activity_inputs VALUES(?1,1,?2,'content','agent/node.amber',?3,?4,11,?5,0)",params![ns.as_str(),format!("local/{n:04}"),serde_json::to_string(&Some("one")).unwrap(),n+1,n.to_string()]).unwrap();
    }
    assert_eq!(
        agent_card_signals::current_activity(&tx, &ns, "agent/node.amber", Some("one")).unwrap(),
        None
    );
    let mut cursor = None;
    let mut visited = 0;
    loop {
        let (page, more) = agent_card_signals::local_cut_page(
            &tx,
            &ns,
            10,
            11,
            cursor
                .as_ref()
                .map(|(at, id): &(u64, String)| (*at, id.as_str())),
            128,
        )
        .unwrap();
        assert!(page.len() <= 128);
        if let Some((at, id, _)) = page.last() {
            cursor = Some((*at, id.clone()));
        }
        for (_, id, _) in &page {
            agent_card_signals::repair_local_cut(&tx, &ns, id, 11).unwrap();
        }
        visited += page.len();
        if !more {
            break;
        }
    }
    assert_eq!(visited, 4096);
    assert_eq!(
        agent_card_signals::current_activity(&tx, &ns, "agent/node.amber", Some("one")).unwrap(),
        Some(4095)
    );
    tx.rollback().unwrap();
}

#[test]
fn local_scan_page_before_clock_does_not_adopt_a_future_anchor() {
    let store = seed();
    let ns = context(&store);
    let index = store.index().unwrap();
    {
        let mut w = store.connection.write();
        let tx = w.transaction().unwrap();
        tx.execute("INSERT INTO local_observations(after_store_index,subject,kind,body,observed_at_unix_ms) VALUES(?1,'agent/node.amber','harness.timeline',?2,1)",params![index+100,json!({"fields":{"incarnation_id":"one","entry_type":"message"}}).to_string()]).unwrap();
        tx.commit().unwrap();
    }
    let at = clock(&store);
    let kernel = Kernel::new("node");
    let inputs = capture(&store);
    let (clock, other): (Vec<_>, Vec<_>) = inputs
        .into_iter()
        .partition(|m| key_parts(&m.key).unwrap().0 == "local_agent_card_clock");
    for page in other.chunks(WORK) {
        let mut w = store.connection.write();
        let tx = w.transaction().unwrap();
        kernel.apply(&tx, &ns, page).unwrap();
        tx.commit().unwrap();
    }
    {
        let mut w = store.connection.write();
        let tx = w.transaction().unwrap();
        kernel.apply(&tx, &ns, &clock).unwrap();
        tx.commit().unwrap();
    }
    drain(&store, &ns, &kernel);
    compare(&store, &ns, at);
    let c = store.readers.get();
    assert_eq!(
        c.query_row(
            "SELECT eligible FROM local_agent_card_activity_inputs WHERE namespace=?1 AND source=1",
            [ns.as_str()],
            |r| r.get::<_, bool>(0)
        )
        .unwrap(),
        false
    );
}
