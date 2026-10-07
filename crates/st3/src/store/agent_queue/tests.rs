use super::*;
use smallclaims::ivm::install::{Installer, Limits, Mutation, Operator, Outcome, ScanPage};

// Obtain real opaque Namespace tokens through the supported Installer lifecycle. This
// empty token source is test-only and does not certify the Store's production dependencies.
struct Token;
impl Operator for Token {
    fn name(&self) -> &'static str {
        "test.queue-token"
    }
    fn fingerprint(&self) -> &'static str {
        "test.empty-source-token.v1"
    }
    fn source(&self) -> &'static str {
        "test.empty-token-source"
    }
    fn create_schema(&self, connection: &Connection) -> Result<()> {
        super::create_schema(connection)
    }
    fn apply(&self, tx: &Transaction<'_>, ns: &Namespace, _: &[Mutation]) -> Result<bool> {
        clock(tx, ns.as_str())?;
        Ok(false)
    }
    fn validate_publication(&self, _: &Transaction<'_>, _: &Namespace) -> Result<()> {
        Ok(())
    }
    fn reclaim(&self, tx: &Transaction<'_>, ns: &Namespace, rows: usize) -> Result<bool> {
        super::reclaim(tx, ns, rows)
    }
}
fn namespace(connection: &mut Connection) -> Namespace {
    let installer = Installer::new(vec![Box::new(Token)]).unwrap();
    installer.create_schema(connection).unwrap();
    let tx = connection.transaction().unwrap();
    if installer.position(&tx, "test.empty-token-source").is_err() {
        installer
            .register_source(&tx, "test.empty-token-source", "test.empty.v1", 1)
            .unwrap();
    }
    let job = installer
        .start(
            &tx,
            "test.queue-token",
            Limits {
                page_rows: 128,
                page_bytes: 1024 * 1024,
                pending_rows: 10000,
                pending_bytes: 1024 * 1024,
                total_rows: 10000,
                callback_ms: 1000,
                lifetime_ms: 60000,
            },
            0,
        )
        .unwrap();
    let position = installer.position(&tx, "test.empty-token-source").unwrap();
    installer
        .scan(
            &tx,
            &ScanPage {
                job: job.clone(),
                expected_cursor: vec![],
                next_cursor: vec![1],
                position,
                rows: vec![],
                finished: true,
            },
            0,
        )
        .unwrap();
    assert_eq!(
        installer.catch_up(&tx, &job, 0).unwrap(),
        Outcome::Published
    );
    let ns = installer.root(&tx, "test.queue-token").unwrap().namespace;
    tx.commit().unwrap();
    ns
}

// This full extraction is strictly a real-Store test oracle. Production applies bounded
// complete replacements from the shared installable source namespace, never this scanner.
fn projected(connection: &Connection, table: &str, key: &str) -> BTreeMap<String, Value> {
    let mut statement = connection
        .prepare(&format!("SELECT * FROM {table} ORDER BY {key}"))
        .unwrap();
    let columns = statement
        .column_names()
        .iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>();
    statement
        .query_map([], |row| {
            let mut body = serde_json::Map::new();
            for (index, name) in columns.iter().enumerate() {
                let value = match row.get_ref(index)? {
                    rusqlite::types::ValueRef::Null => Value::Null,
                    rusqlite::types::ValueRef::Integer(n) => json!(n),
                    rusqlite::types::ValueRef::Real(n) => json!(n),
                    rusqlite::types::ValueRef::Text(s) => json!(std::str::from_utf8(s).unwrap()),
                    rusqlite::types::ValueRef::Blob(_) => panic!("unexpected projected queue blob"),
                };
                body.insert(name.clone(), value);
            }
            let value = Value::Object(body);
            Ok((value[key].as_str().unwrap().to_owned(), value))
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}
#[derive(Default)]
struct Source {
    steps: BTreeMap<String, Value>,
    runs: BTreeMap<String, Value>,
    claims: BTreeMap<String, Value>,
}
struct ResetClock;
impl Drop for ResetClock {
    fn drop(&mut self) {
        smallclaims::store::set_thread_clock(None);
    }
}
impl Source {
    fn capture(&mut self, store: &Store, ns: &Namespace) {
        let mut writer = store.connection.write();
        let tx = writer.transaction().unwrap();
        let steps = projected(&tx, "step_runs", "subject");
        let runs = projected(&tx, "mission_runs", "id");
        for key in self.steps.keys().filter(|key| !steps.contains_key(*key)) {
            assert_eq!(
                replace_step(&tx, ns, key, None).unwrap(),
                Coverage::Complete
            );
        }
        for (key, new) in &steps {
            if self.steps.get(key) != Some(new) {
                assert_eq!(
                    replace_step(&tx, ns, key, Some(new)).unwrap(),
                    Coverage::Complete
                );
            }
        }
        for key in self.runs.keys().filter(|key| !runs.contains_key(*key)) {
            assert_eq!(replace_run(&tx, ns, key, None).unwrap(), Coverage::Complete);
        }
        for (key, new) in &runs {
            if self.runs.get(key) != Some(new) {
                assert_eq!(
                    replace_run(&tx, ns, key, Some(new)).unwrap(),
                    Coverage::Complete
                );
            }
        }
        let claims=tx.prepare("SELECT id,subject,kind,body FROM claims WHERE kind IN ('agent.queue.moved','step-run.carried','work.claimed') ORDER BY store_index").unwrap()
   .query_map([],|row|Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,String>(2)?,row.get::<_,String>(3)?))).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap()
   .into_iter().map(|(id,subject,kind,body)|{
    let rank=canonical::sortable_key(&canonical::claim_key(&tx,&id).unwrap());
    let repaired:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM replica_records WHERE claim_id=?1 AND state='repaired')",[&id],|row|row.get(0)).unwrap();
    let eligible=kind!="agent.queue.moved"||!repaired;
    let body:Value=serde_json::from_str(&body).unwrap();
    (id.clone(),json!({"id":id,"subject":subject,"kind":kind,"rank":rank,"body":body,"eligible":eligible}))
   }).collect::<BTreeMap<_,_>>();
        for key in self.claims.keys().filter(|key| !claims.contains_key(*key)) {
            assert_eq!(
                replace_claim(&tx, ns, key, None).unwrap(),
                Coverage::Complete
            );
        }
        for (key, new) in &claims {
            if self.claims.get(key) != Some(new) {
                assert_eq!(
                    replace_claim(&tx, ns, key, Some(new)).unwrap(),
                    Coverage::Complete
                );
            }
        }
        tx.commit().unwrap();
        self.steps = steps;
        self.runs = runs;
        self.claims = claims;
    }
    fn settle(&self, store: &Store, ns: &Namespace, at: u128, limit: usize) -> usize {
        let mut processed = 0;
        for _ in 0..20000 {
            let mut writer = store.connection.write();
            let tx = writer.transaction().unwrap();
            let result = drain(&tx, ns, at, limit).unwrap();
            assert_eq!(result.coverage, Coverage::Complete);
            assert!(result.processed <= limit);
            processed += result.processed;
            if result.clean {
                tx.commit().unwrap();
                return processed;
            }
            assert!(rows(&tx, ns, &[], at).is_err());
            tx.commit().unwrap();
        }
        panic!("queue dependency drain did not settle")
    }
    fn parity(&self, store: &Store, ns: &Namespace, at: u128, agents: &[String]) {
        smallclaims::store::set_thread_clock(Some(at));
        let _reset_clock = ResetClock;
        let _clock = clock_snapshot();
        let expected = store
            .agent_work_queues()
            .unwrap()
            .into_iter()
            .filter(|(agent, _)| agents.contains(agent))
            .collect::<BTreeMap<_, _>>();
        let connection = store.readers.get();
        assert_eq!(rows(&connection, ns, agents, at).unwrap(), expected);
        let subjects = expected
            .values()
            .flat_map(|row| {
                row.current_work_ids
                    .iter()
                    .chain(row.upcoming_work_ids.iter())
            })
            .cloned()
            .collect::<Vec<_>>();
        let old = store.step_labels(&subjects).unwrap();
        let new = labels(&connection, ns, &subjects).unwrap();
        assert_eq!(new.len(), old.len());
        for (key, old) in old {
            let new = &new[&key];
            assert_eq!(
                (
                    &new.run,
                    &new.mission,
                    &new.path,
                    &new.title,
                    &new.goal,
                    &new.status,
                    new.updated_at_unix_ms
                ),
                (
                    &old.run,
                    &old.mission,
                    &old.path,
                    &old.title,
                    &old.goal,
                    &old.status,
                    old.updated_at_unix_ms
                )
            );
        }
    }
}
fn fixture() -> (Store, Namespace, Vec<MissionRunView>, Vec<String>) {
    let store = Store::open_memory("cedar").unwrap();
    let intent=crate::parse_intent("version 2\nmission \"queue-sample\" state=\"ready\" { concurrent-runs max=8; goal \"Prepare samples.\"; step \"build\" { assigned-to \"agent/worker\"; title \"Build sample\"; goal \"First goal\"; goal \"Second goal\"; }; step \"review\" { assigned-to \"agent/spruce\"; goal \"Review sample\"; } }\n",store.origin()).unwrap();
    store.apply_internal(&intent, "publish").unwrap();
    let mut runs = Vec::new();
    for number in 0..7 {
        runs.push(
            store
                .create_mission_run(&MissionRunRequest {
                    mission: "queue-sample".into(),
                    revision: None,
                    workspace: "/example/project".into(),
                    requester: Some("person/avery".into()),
                    mode: None,
                    inputs: BTreeMap::new(),
                    idempotency_key: format!("start-{number}"),
                })
                .unwrap(),
        );
    }
    for run in &runs {
        for step in &run.steps {
            store.set_step_state(&step.subject, "ready", None).unwrap();
        }
    }
    let agents = runs[0]
        .steps
        .iter()
        .filter_map(|step| step.assigned_to.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let ns = {
        let mut writer = store.connection.write();
        namespace(&mut writer)
    };
    (store, ns, runs, agents)
}
#[test]
fn real_store_counts_prefix_labels_moves_and_terminal_membership() {
    let (store, ns, runs, agents) = fixture();
    let mut source = Source::default();
    source.capture(&store, &ns);
    assert!(rows(&store.readers.get(), &ns, &agents, now_ms()).is_err());
    source.settle(&store, &ns, now_ms(), 2);
    source.parity(&store, &ns, now_ms(), &agents);
    let queues = rows(&store.readers.get(), &ns, &agents, now_ms()).unwrap();
    assert_eq!(queues.len(), 2);
    assert!(
        queues
            .values()
            .all(|queue| queue.upcoming_work_ids.len() == AGENT_WORK_PREVIEW_LIMIT)
    );
    assert!(queues.values().all(|queue| queue.queued_work_count == 7));
    let agent = runs[0].steps[0].assigned_to.clone().unwrap();
    store
        .move_seat_queue_run(&SeatQueueMoveRequest {
            agent,
            run: runs[3].subject.clone(),
            placement: "top".into(),
            anchor: None,
            reason: Some("invented fixture".into()),
            actor: "person/avery".into(),
            idempotency_key: "last-top".into(),
        })
        .unwrap();
    source.capture(&store, &ns);
    source.settle(&store, &ns, now_ms(), 1);
    source.parity(&store, &ns, now_ms(), &agents);
    store
        .set_step_state(&runs[0].steps[0].subject, "completed", None)
        .unwrap();
    source.capture(&store, &ns);
    source.settle(&store, &ns, now_ms(), 2);
    source.parity(&store, &ns, now_ms(), &agents);
    store
        .set_mission_run_state(&runs[3].id, "cancelled", "terminal", None)
        .unwrap();
    source.capture(&store, &ns);
    source.settle(&store, &ns, now_ms(), 2);
    source.parity(&store, &ns, now_ms(), &agents);
    assert!(
        rows(
            &store.readers.get(),
            &ns,
            &vec!["agent/other".into(); 502],
            now_ms()
        )
        .is_err()
    );
    assert!(
        rows(&store.readers.get(), &ns, &["agent/other".into()], now_ms())
            .unwrap()
            .is_empty()
    );
}
#[test]
fn nested_ready_parent_and_submitted_parent_follow_existing_selector() {
    let store = Store::open_memory("cedar").unwrap();
    let intent=crate::parse_intent("version 2\nmission \"nested-sample\" state=\"ready\" { goal \"Prepare samples.\"; step \"parent\" { assigned-to \"agent/worker\"; goal \"Build sample\"; mission \"work\" { goal \"Review sample\"; step \"child\" { goal \"Review sample\"; } } }; step \"other\" { assigned-to \"agent/worker\"; goal \"Another sample\"; } }\n",store.origin()).unwrap();
    store.apply_internal(&intent, "publish").unwrap();
    let run = store
        .create_mission_run(&MissionRunRequest {
            mission: "nested-sample".into(),
            revision: None,
            workspace: "/example/project".into(),
            requester: Some("person/avery".into()),
            mode: None,
            inputs: BTreeMap::new(),
            idempotency_key: "start".into(),
        })
        .unwrap();
    let agent = run.steps[0].assigned_to.clone().unwrap();
    let ns = {
        let mut writer = store.connection.write();
        namespace(&mut writer)
    };
    let parent = run.steps.iter().find(|step| step.step == "parent").unwrap();
    let child = run
        .steps
        .iter()
        .find(|step| step.step == "parent/work/child")
        .unwrap();
    store.set_step_state(&child.subject, "ready", None).unwrap();
    store
        .set_step_state(&parent.subject, "ready", None)
        .unwrap();
    let mut source = Source::default();
    source.capture(&store, &ns);
    source.settle(&store, &ns, now_ms(), 1);
    source.parity(&store, &ns, now_ms(), &[agent.clone()]);
    assert!(
        !rows(&store.readers.get(), &ns, &[agent.clone()], now_ms()).unwrap()[&agent]
            .upcoming_work_ids
            .contains(&child.subject)
    );
    store
        .set_step_state(&parent.subject, "verifying", None)
        .unwrap();
    source.capture(&store, &ns);
    source.settle(&store, &ns, now_ms(), 1);
    source.parity(&store, &ns, now_ms(), &[agent.clone()]);
    let queue = &rows(&store.readers.get(), &ns, &[agent.clone()], now_ms()).unwrap()[&agent];
    assert_eq!(queue.active_work_count, 0);
    assert!(queue.upcoming_work_ids.contains(&child.subject));
    store
        .set_step_state(&child.subject, "completed", None)
        .unwrap();
    source.capture(&store, &ns);
    source.settle(&store, &ns, now_ms(), 1);
    source.parity(&store, &ns, now_ms(), &[agent]);
}

#[test]
fn normal_claim_and_inclusive_lease_deadline_require_writer_maintenance() {
    let (store, ns, _, agents) = fixture();
    let mut source = Source::default();
    source.capture(&store, &ns);
    source.settle(&store, &ns, now_ms(), 2);
    let queues = store.agent_work_queues().unwrap();
    let agent = &agents[0];
    let subject = queues[agent].next_work_id.as_ref().unwrap();
    let claimed = store
        .work_action(
            subject,
            "claim",
            &WorkRequest {
                actor: Some(agent.clone()),
                incarnation: Some("sample-incarnation".into()),
                summary: None,
                reason: None,
                evidence: vec![],
                idempotency_key: "sample-claim".into(),
            },
        )
        .unwrap();
    let expiry = claimed.claim_expires_at_unix_ms.unwrap();
    source.capture(&store, &ns);
    source.settle(&store, &ns, expiry - 1, 1);
    source.parity(&store, &ns, expiry - 1, &agents);
    assert_eq!(
        next_deadline(&store.readers.get(), &ns).unwrap(),
        Some(expiry)
    );
    assert!(rows(&store.readers.get(), &ns, &agents, expiry).is_err());
    // A read at the exact expiry must not repair the dependency or serve stale held work.
    assert_eq!(
        next_deadline(&store.readers.get(), &ns).unwrap(),
        Some(expiry)
    );
    source.settle(&store, &ns, expiry, 1);
    source.parity(&store, &ns, expiry, &agents);
    assert_eq!(next_deadline(&store.readers.get(), &ns).unwrap(), None);
    assert_eq!(
        rows(&store.readers.get(), &ns, &[agent.clone()], expiry).unwrap()[agent].active_work_count,
        0
    );
}

#[test]
fn source_rollback_logical_unsupported_and_selected_query_plans() {
    let (store, ns, _, agents) = fixture();
    let mut source = Source::default();
    source.capture(&store, &ns);
    let at = now_ms();
    source.settle(&store, &ns, at, 2);
    let before = rows(&store.readers.get(), &ns, &agents, at).unwrap();
    let subject = source.steps.keys().next().unwrap();
    let mut changed = source.steps[subject].clone();
    changed["assignee"] = json!("agent/reassigned");
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    assert_eq!(
        replace_step(&tx, &ns, subject, Some(&changed)).unwrap(),
        Coverage::Complete
    );
    assert!(rows(&tx, &ns, &agents, at).is_err());
    while !drain(&tx, &ns, at, 1).unwrap().clean {}
    assert_ne!(rows(&tx, &ns, &agents, at).unwrap(), before);
    tx.rollback().unwrap();
    assert_eq!(rows(&writer, &ns, &agents, at).unwrap(), before);
    let tx = writer.transaction().unwrap();
    changed["agentless"] = json!("wrong SQL type");
    assert!(matches!(
        replace_step(&tx, &ns, subject, Some(&changed)).unwrap(),
        Coverage::Unsupported(_)
    ));
    assert_eq!(rows(&tx, &ns, &agents, at).unwrap(), before);
    changed["agentless"] = source.steps[subject]["agentless"].clone();
    changed["title"] = json!({"tagged_blob":"00"});
    assert!(matches!(
        replace_step(&tx, &ns, subject, Some(&changed)).unwrap(),
        Coverage::Unsupported(_)
    ));
    changed["title"] = source.steps[subject]["title"].clone();
    changed["goals"] = json!("not JSON");
    assert!(matches!(
        replace_step(&tx, &ns, subject, Some(&changed)).unwrap(),
        Coverage::Unsupported(_)
    ));
    changed["goals"] = source.steps[subject]["goals"].clone();
    changed["updated_at_unix_ms"] = json!("not a timestamp");
    assert!(matches!(
        replace_step(&tx, &ns, subject, Some(&changed)).unwrap(),
        Coverage::Unsupported(_)
    ));
    let (run, raw) = source.runs.iter().next().unwrap();
    let mut invalid_run = raw.clone();
    invalid_run["mission_id"] = json!({"tagged_blob":"00"});
    assert!(matches!(
        replace_run(&tx, &ns, run, Some(&invalid_run)).unwrap(),
        Coverage::Unsupported(_)
    ));
    assert_eq!(rows(&tx, &ns, &agents, at).unwrap(), before);
    // Logical gaps are returned to the shared source owner, which can commit and fence.
    tx.commit().unwrap();
    let plan = writer
        .prepare(&format!("EXPLAIN QUERY PLAN {ROWS}"))
        .unwrap()
        .query_map(
            params![ns.as_str(), serde_json::to_string(&agents).unwrap()],
            |row| row.get::<_, String>(3),
        )
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
        .join("\n");
    assert!(plan.contains("local_agent_queue_held"), "{plan}");
    assert!(plan.contains("local_agent_queue_ready"), "{plan}");
    assert!(!plan.contains("TEMP B-TREE"), "{plan}");
    assert!(rows(&writer, &ns, &["person/avery".into()], at).is_err());
}

#[test]
fn namespace_cleanup_respects_total_delete_budget_and_other_namespace() {
    let (store, ns, _, agents) = fixture();
    let mut source = Source::default();
    source.capture(&store, &ns);
    let at = now_ms();
    source.settle(&store, &ns, at, 2);
    let other = {
        let mut writer = store.connection.write();
        namespace(&mut writer)
    };
    let mut other_source = Source::default();
    other_source.capture(&store, &other);
    other_source.settle(&store, &other, at, 2);
    let expected = rows(&store.readers.get(), &other, &agents, at).unwrap();
    let mut writer = store.connection.write();
    let tables = writer.prepare("SELECT name FROM sqlite_master WHERE type='table' AND (name LIKE 'local_agent_queue_%' OR name LIKE 'local_seat_order_%')").unwrap()
        .query_map([], |r|r.get::<_,String>(0)).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap();
    writer.execute_batch("CREATE TEMP TABLE queue_delete_witness(n INTEGER NOT NULL); INSERT INTO queue_delete_witness VALUES(0);").unwrap();
    for table in &tables {
        writer.execute_batch(&format!("CREATE TEMP TRIGGER witness_{table} AFTER DELETE ON main.{table} BEGIN UPDATE queue_delete_witness SET n=n+1; END;")).unwrap();
    }
    let mut done = false;
    for _ in 0..2000 {
        let tx = writer.transaction().unwrap();
        tx.execute("UPDATE queue_delete_witness SET n=0", [])
            .unwrap();
        done = reclaim(&tx, &ns, 2).unwrap();
        let deleted: i64 = tx
            .query_row("SELECT n FROM queue_delete_witness", [], |r| r.get(0))
            .unwrap();
        assert!(
            deleted <= 2,
            "cleanup deleted {deleted} rows for a two-row budget"
        );
        assert_eq!(rows(&tx, &other, &agents, at).unwrap(), expected);
        tx.commit().unwrap();
        if done {
            break;
        }
    }
    assert!(done);
    for table in tables {
        assert_eq!(
            writer
                .query_row(
                    &format!("SELECT COUNT(*) FROM {table} WHERE namespace=?1"),
                    [ns.as_str()],
                    |r| r.get::<_, usize>(0)
                )
                .unwrap(),
            0
        );
    }
}

#[test]
fn real_generation_cut_and_carried_priority_then_claim_match_existing_reader() {
    let (store, ns, runs, agents) = fixture();
    let first = &runs[0];
    let step = &first.steps[0];
    let agent = step.assigned_to.clone().unwrap();
    let request = |key: &str| WorkRequest {
        actor: Some(agent.clone()),
        incarnation: Some("sample-carry".into()),
        summary: None,
        reason: None,
        evidence: vec![],
        idempotency_key: key.into(),
    };
    store
        .work_action(&step.subject, "claim", &request("claim-before-revision"))
        .unwrap();
    // An accepted native renewed claim is the existing test seam for a finite expired lease;
    // it updates the real source projection rather than mutating private queue output.
    expire_work_lease_by_claim(&store, &step.subject, &agent, "sample-carry");
    let intent=crate::parse_intent("version 2\nmission \"queue-sample\" state=\"ready\" { concurrent-runs max=8; goal \"Prepare samples.\"; step \"build\" { assigned-to \"agent/worker\"; title \"Build sample\"; goal \"First goal\"; goal \"Second goal\"; }; step \"review\" { assigned-to \"agent/spruce\"; goal \"Check sample\"; } }\n",store.origin()).unwrap();
    store.apply_internal(&intent, "publish-second").unwrap();
    let revised = store
        .adopt_mission_revision(
            &first.id,
            &intent.missions["queue-sample"],
            "person/avery",
            "invented revision",
            "sample-revision",
        )
        .unwrap();
    let carried = revised.steps.iter().find(|s| s.step == step.step).unwrap();
    assert_ne!(carried.subject, step.subject);
    assert_eq!(carried.status, "ready");
    let mut source = Source::default();
    source.capture(&store, &ns);
    source.settle(&store, &ns, now_ms(), 1);
    source.parity(&store, &ns, now_ms(), &agents);
    assert_eq!(
        rows(&store.readers.get(), &ns, &[agent.clone()], now_ms()).unwrap()[&agent]
            .next_work_id
            .as_deref(),
        Some(carried.subject.as_str())
    );
    store
        .work_action(&carried.subject, "claim", &request("claim-after-revision"))
        .unwrap();
    source.capture(&store, &ns);
    source.settle(&store, &ns, now_ms(), 1);
    source.parity(&store, &ns, now_ms(), &agents);
    assert_eq!(
        rows(&store.readers.get(), &ns, &[agent.clone()], now_ms()).unwrap()[&agent]
            .active_work_count,
        1
    );
}

#[test]
fn captured_label_only_replacement_invalidates_public_agent_without_queue_delta() {
    let (store, ns, runs, agents) = fixture();
    let mut source = Source::default();
    source.capture(&store, &ns);
    let at = now_ms();
    source.settle(&store, &ns, at, 1);
    let before = rows(&store.readers.get(), &ns, &agents, at).unwrap();
    {
        let mut writer = store.connection.write();
        let tx = writer.transaction().unwrap();
        acknowledge(&tx, &ns, &agents).unwrap();
        // Simulate a complete projection replacement at the capture seam. Direct SQL is
        // restricted to this control; production relies on the shared writer capture owner.
        tx.execute("UPDATE step_runs SET title='Revised sample title',goals='[\"Revised sample goal\"]' WHERE subject=?1", [&runs[0].steps[0].subject]).unwrap();
        tx.commit().unwrap();
    }
    source.capture(&store, &ns);
    source.settle(&store, &ns, at, 1);
    assert_eq!(
        rows(&store.readers.get(), &ns, &agents, at).unwrap(),
        before
    );
    let agent = runs[0].steps[0].assigned_to.as_ref().unwrap();
    assert_eq!(
        dirty_agents(&store.readers.get(), &ns, 1).unwrap(),
        vec![agent.clone()]
    );
    source.parity(&store, &ns, at, &agents);
    let label = labels(
        &store.readers.get(),
        &ns,
        &[runs[0].steps[0].subject.clone()],
    )
    .unwrap();
    assert_eq!(
        label[&runs[0].steps[0].subject].title.as_deref(),
        Some("Revised sample title")
    );
}

#[test]
fn populated_queue_and_labels_match_ordered_permuted_duplicate_replication() {
    use crate::store::tests::{exchange_from, receive_and_project};
    let (store, _, runs, agents) = fixture();
    let agent = runs[0].steps[0].assigned_to.as_ref().unwrap();
    store
        .move_seat_queue_run(&SeatQueueMoveRequest {
            agent: agent.clone(),
            run: runs[5].subject.clone(),
            placement: "top".into(),
            anchor: None,
            reason: Some("invented fixture".into()),
            actor: "person/avery".into(),
            idempotency_key: "replicated-move".into(),
        })
        .unwrap();
    let subject = store.agent_work_queues().unwrap()[agent]
        .next_work_id
        .clone()
        .unwrap();
    store
        .work_action(
            &subject,
            "claim",
            &WorkRequest {
                actor: Some(agent.clone()),
                incarnation: Some("replicated-sample".into()),
                summary: None,
                reason: None,
                evidence: vec![],
                idempotency_key: "replicated-claim".into(),
            },
        )
        .unwrap();
    let exchange = exchange_from(&store, &ReplicationInventory::default());
    let at = now_ms();
    for reverse in [false, true] {
        let target = Store::open_memory("birch").unwrap();
        let mut exchange = exchange.clone();
        if reverse {
            exchange.envelopes.reverse();
        }
        receive_and_project(&target, "cedar", &exchange);
        let ns = {
            let mut writer = target.connection.write();
            namespace(&mut writer)
        };
        let mut source = Source::default();
        source.capture(&target, &ns);
        source.settle(&target, &ns, at, 2);
        source.parity(&target, &ns, at, &agents);
        assert_eq!(
            rows(&target.readers.get(), &ns, &agents, at).unwrap(),
            store.agent_work_queues().unwrap()
        );
        receive_and_project(&target, "cedar", &exchange);
        source.capture(&target, &ns);
        assert_eq!(source.settle(&target, &ns, at, 2), 0);
        source.parity(&target, &ns, at, &agents);
    }
}

#[test]
fn real_store_reopen_preserves_pending_component_and_requires_writer_resume() {
    use crate::store::tests::{exchange_from, receive_and_project};
    let (store, _, _, agents) = fixture();
    let exchange = exchange_from(&store, &ReplicationInventory::default());
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("queue.sqlite3");
    let target = Store::open(&path, "birch").unwrap();
    receive_and_project(&target, "cedar", &exchange);
    let ns = {
        let mut writer = target.connection.write();
        namespace(&mut writer)
    };
    let mut source = Source::default();
    source.capture(&target, &ns);
    let at = now_ms();
    {
        let mut writer = target.connection.write();
        let tx = writer.transaction().unwrap();
        let page = drain(&tx, &ns, at, 1).unwrap();
        assert_eq!(page.processed, 1);
        assert!(!page.clean);
        tx.commit().unwrap();
    }
    assert!(rows(&target.readers.get(), &ns, &agents, at).is_err());
    drop(target);
    let target = Store::open(&path, "birch").unwrap();
    assert!(rows(&target.readers.get(), &ns, &agents, at).is_err());
    assert!(!clean(&target.readers.get(), &ns, at).unwrap());
    source.settle(&target, &ns, at, 1);
    source.parity(&target, &ns, at, &agents);
    assert!(
        rows(&target.readers.get(), &ns, &agents, at)
            .unwrap()
            .values()
            .all(|queue| queue.queued_work_count == 7)
    );
}
