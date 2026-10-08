use super::*;
use crate::model::PersonStepResponse;
use smallclaims::ivm::install::{Installer, Limits, Mutation, Operator, ScanPage};

// Obtain the foundation's opaque namespace without publishing a view or source certificate.
fn namespace(store: &Store, view: &'static str) -> Namespace {
    struct Probe(&'static str, Arc<Mutex<Option<Namespace>>>);
    impl Operator for Probe {
        fn name(&self) -> &'static str {
            self.0
        }
        fn source(&self) -> &'static str {
            "fixture/human-source"
        }
        fn fingerprint(&self) -> &'static str {
            "fixture/human.v1"
        }
        fn create_schema(&self, c: &Connection) -> Result<()> {
            create_schema(c)
        }
        fn apply(
            &self,
            _: &Transaction<'_>,
            namespace: &Namespace,
            _: &[Mutation],
        ) -> Result<bool> {
            *self.1.lock().unwrap() = Some(namespace.clone());
            Ok(false)
        }
        fn validate_publication(&self, _: &Transaction<'_>, _: &Namespace) -> Result<()> {
            anyhow::bail!("fixture is not a source certificate")
        }
        fn reclaim(&self, tx: &Transaction<'_>, ns: &Namespace, limit: usize) -> Result<bool> {
            reclaim(tx, ns, limit)
        }
    }
    let captured = Arc::new(Mutex::new(None));
    let installer = Installer::new(vec![Box::new(Probe(view, captured.clone()))]).unwrap();
    installer.create_schema(&store.connection.write()).unwrap();
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    if installer.position(&tx, "fixture/human-source").is_err() {
        installer
            .register_source(&tx, "fixture/human-source", "fixture/human.v1", 1)
            .unwrap();
    }
    let position = installer.position(&tx, "fixture/human-source").unwrap();
    let job = installer
        .start(
            &tx,
            view,
            Limits {
                page_rows: 128,
                page_bytes: 1024 * 1024,
                pending_rows: 128,
                pending_bytes: 1024 * 1024,
                total_rows: 1024,
                callback_ms: 1000,
                lifetime_ms: 60_000,
            },
            0,
        )
        .unwrap();
    installer
        .scan(
            &tx,
            &ScanPage {
                job,
                expected_cursor: vec![],
                next_cursor: vec![1],
                position,
                rows: vec![Mutation {
                    key: "fixture/row".into(),
                    old: None,
                    new: Some(json!({"value":1})),
                }],
                finished: false,
            },
            0,
        )
        .unwrap();
    tx.commit().unwrap();
    captured.lock().unwrap().clone().unwrap()
}

fn family(kind: &str) -> Family {
    match kind {
        "person-step" => Family::Person,
        "human-gate" => Family::Review,
        "launch-approval" => Family::Planning,
        "revision-approval" => Family::Revision,
        _ => panic!("unexpected human kind {kind}"),
    }
}

// Native full computation is only a fixture input/oracle, never a production materializer.
fn refresh(store: &Store, ns: &Namespace, at: u128) -> BTreeSet<String> {
    let inputs = store
        .read_snapshot(|_| {
            let mut items = store.mission_run_attention_items(None)?;
            items.extend(store.person_attention_items(None, u128::MAX)?);
            let mut groups = BTreeMap::<(String, String), Vec<AttentionItemView>>::new();
            for item in items {
                groups
                    .entry((item.kind.clone(), item.subject.clone()))
                    .or_default()
                    .push(item);
            }
            let c = store.readers.get();
            let mut inputs = vec![];
            for ((kind, source), items) in groups {
                let family = family(&kind);
                let accepted = if family == Family::Person {
                    person_work::request(&c, &source)?.map(|ask| ask.accepted_at_unix_ms)
                } else {
                    None
                };
                inputs.push(Input::from_items(family, source, &items, accepted)?);
            }
            Ok(inputs)
        })
        .unwrap();
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    let prior: Vec<(String,String)> = tx.prepare("SELECT DISTINCT family,source FROM local_attention_human_membership WHERE namespace=?1 ORDER BY family,source").unwrap().query_map([ns.as_str()],|row|Ok((row.get(0)?,row.get(1)?))).unwrap().collect::<rusqlite::Result<_>>().unwrap();
    let next: BTreeSet<_> = inputs
        .iter()
        .map(|input| (input.family.name().to_owned(), input.source.clone()))
        .collect();
    for (name, source) in prior {
        if !next.contains(&(name.clone(), source.clone())) {
            let family = match name.as_str() {
                "person" => Family::Person,
                "review" => Family::Review,
                "planning" => Family::Planning,
                "revision" => Family::Revision,
                _ => panic!("bad family"),
            };
            replace(
                &tx,
                ns,
                &Input {
                    family,
                    source,
                    runs: vec![],
                },
            )
            .unwrap();
        }
    }
    for input in inputs {
        replace(&tx, ns, &input).unwrap();
    }
    let actual: BTreeSet<String> = tx.prepare("SELECT DISTINCT run FROM local_attention_human_membership WHERE namespace=?1 AND eligible<=?2 ORDER BY run").unwrap().query_map(params![ns.as_str(),at.to_be_bytes().to_vec()],|row|row.get(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap();
    tx.commit().unwrap();
    drop(writer);
    let mut expected = store.mission_run_attention_items(None).unwrap();
    expected.extend(store.person_attention_items(None, at).unwrap());
    assert_eq!(
        actual,
        expected
            .into_iter()
            .filter_map(|item| item.mission_run)
            .collect()
    );
    actual
}

fn append(store: &Store, subject: &str, kind: &str, actor: &str, fields: Value) -> ClaimRecord {
    store
        .append_claim(&ClaimInput {
            subject: subject.into(),
            kind: kind.into(),
            actor: Some(actor.into()),
            fields: serde_json::from_value(fields).unwrap(),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap()
}

#[test]
fn native_person_and_future_review_membership_match_before_public_visibility_and_after_answer() {
    let (store, origin, request) = person_work::tests::fixture();
    let ns = namespace(&store, "fixture/human-one");
    let now = now_ms();
    assert!(refresh(&store, &ns, now).is_empty());
    let asked = store.ask_person(&request).unwrap();
    assert_eq!(
        refresh(&store, &ns, now_ms()),
        store.human_attention_runs().unwrap()
    );
    store
        .finish_person_step(
            &PersonStepResponse {
                delegation: None,
                subject: asked.subject,
                actor: "person/avery".into(),
                summary: "Reviewed".into(),
                evidence: vec![],
                episode: None,
                idempotency_key: "human-answer".into(),
                answer: None,
            },
            false,
        )
        .unwrap();
    assert!(refresh(&store, &ns, now_ms()).is_empty());
    let run = store.mission_run(&origin.run).unwrap().unwrap();
    let future = now_ms() + 60_000;
    store.set_write_clock_at(future).unwrap();
    let review = append(
        &store,
        "gate-operation/fixture-human-future",
        "gate.requested",
        "agent/alder.asker",
        json!({"owner":origin.subject,"reviewer":"person/avery","mission_revision":run.revision,"step_definition":origin.definition_hash,"attempt":origin.attempt,"question":"Approve invented review?","mode":"approve"}),
    );
    assert!(store.attention_snapshot(None, now_ms()).unwrap().is_empty());
    assert_eq!(
        refresh(&store, &ns, now_ms()),
        BTreeSet::from([origin.run.clone()])
    );
    assert_eq!(
        store.human_attention_runs().unwrap(),
        BTreeSet::from([origin.run.clone()])
    );
    append(
        &store,
        "gate-operation/fixture-human-future",
        "gate.result",
        "person/avery",
        json!({"request":review.id,"verdict":"pass"}),
    );
    assert!(refresh(&store, &ns, now_ms()).is_empty());
}

#[test]
fn replacements_preserve_old_and_new_keys_rollback_namespace_isolation_and_clock_boundaries() {
    let store = Store::open_memory("alder").unwrap();
    let one = namespace(&store, "fixture/human-one");
    let two = namespace(&store, "fixture/human-two");
    let old = "mission-run/fixture-old".to_owned();
    let new = "mission-run/fixture-new".to_owned();
    let mut input = Input {
        family: Family::Person,
        source: "step-run/fixture-human".into(),
        runs: vec![Membership {
            run: old.clone(),
            eligible: u128::MAX,
        }],
    };
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    let changed = replace(&tx, &one, &input).unwrap();
    assert_eq!(changed.affected_runs, BTreeSet::from([old.clone()]));
    assert_eq!(changed.writes, 1);
    assert_eq!(replace(&tx, &one, &input).unwrap().writes, 0);
    let selected = selected(&tx, &one, &[old.clone()], u128::MAX - 1).unwrap();
    assert!(!selected[0].waiting);
    assert_eq!(selected[0].run, old);
    assert_eq!(selected[0].dependencies, BTreeSet::from([old.clone()]));
    assert_eq!(selected[0].next_deadline, Some(u128::MAX));
    assert!(super::selected(&tx, &one, &[old.clone()], u128::MAX).unwrap()[0].waiting);
    assert!(!super::selected(&tx, &two, &[old.clone()], u128::MAX).unwrap()[0].waiting);
    tx.commit().unwrap();
    let tx = writer.transaction().unwrap();
    input.runs[0].run = new.clone();
    assert_eq!(
        replace(&tx, &one, &input).unwrap().affected_runs,
        BTreeSet::from([old.clone(), new.clone()])
    );
    tx.rollback().unwrap();
    assert!(super::selected(&writer, &one, &[old.clone()], u128::MAX).unwrap()[0].waiting);
    assert!(!super::selected(&writer, &one, &[new.clone()], u128::MAX).unwrap()[0].waiting);
    let tx = writer.transaction().unwrap();
    replace(&tx, &two, &input).unwrap();
    assert!(reclaim(&tx, &one, 1).unwrap());
    assert!(super::selected(&tx, &two, &[new], u128::MAX).unwrap()[0].waiting);
    tx.commit().unwrap();
}

#[test]
fn human_and_stop_arrangements_ignore_temp_shadows_for_reads_and_mutations() {
    use crate::store::attention_stop_heads as stops;
    let store = Store::open_memory("alder").unwrap();
    let ns = namespace(&store, "fixture/human-main-storage");
    let mut writer = store.connection.write();
    stops::create_schema(&writer).unwrap();
    let old_run = "mission-run/main-old".to_owned();
    let new_run = "mission-run/main-new".to_owned();
    let head = stops::Head {
        requester: "agent/asker".into(),
        key: (10, "writer".into(), 1, "batch".into(), 0, "stop".into()),
    };
    {
        let tx = writer.transaction().unwrap();
        replace(
            &tx,
            &ns,
            &Input {
                family: Family::Person,
                source: "step-run/main".into(),
                runs: vec![Membership {
                    run: old_run.clone(),
                    eligible: 1,
                }],
            },
        )
        .unwrap();
        stops::replace(&tx, &ns, "stop", Some(&head)).unwrap();
        tx.commit().unwrap();
    }
    writer
        .execute_batch(
            "CREATE TEMP TABLE local_attention_human_membership AS
           SELECT * FROM main.local_attention_human_membership;
         CREATE TEMP TABLE local_attention_stop_heads AS
           SELECT * FROM main.local_attention_stop_heads;
         UPDATE temp.local_attention_stop_heads SET fact='[]';",
        )
        .unwrap();
    writer
        .execute(
            "UPDATE temp.local_attention_human_membership SET eligible=?1",
            [u128::MAX.to_be_bytes().to_vec()],
        )
        .unwrap();
    // Explicit setup and both readers keep main as their physical storage even
    // though the TEMP tables disagree and lack the required keys and indexes.
    create_schema(&writer).unwrap();
    stops::create_schema(&writer).unwrap();
    assert!(selected(&writer, &ns, std::slice::from_ref(&old_run), 1).unwrap()[0].waiting);
    assert_eq!(
        stops::maximum(&writer, &ns, "agent/asker").unwrap(),
        Some(head)
    );
    let tx = writer.transaction().unwrap();
    let changes = replace(
        &tx,
        &ns,
        &Input {
            family: Family::Person,
            source: "step-run/main".into(),
            runs: vec![Membership {
                run: new_run.clone(),
                eligible: 2,
            }],
        },
    )
    .unwrap();
    assert_eq!(
        changes.affected_runs,
        BTreeSet::from([old_run.clone(), new_run.clone()])
    );
    assert_eq!(changes.writes, 2);
    let rows = selected(&tx, &ns, &[old_run, new_run.clone()], 1).unwrap();
    assert!(!rows[0].waiting);
    assert!(!rows[1].waiting);
    assert_eq!(rows[1].next_deadline, Some(2));
    assert!(reclaim(&tx, &ns, 1).unwrap());
    assert!(!selected(&tx, &ns, &[new_run], u128::MAX).unwrap()[0].waiting);
    assert_eq!(stops::replace(&tx, &ns, "stop", None).unwrap().writes, 1);
    assert!(stops::maximum(&tx, &ns, "agent/asker").unwrap().is_none());
    let shadow_eligible: Vec<u8> = tx
        .query_row(
            "SELECT eligible FROM temp.local_attention_human_membership",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(shadow_eligible, u128::MAX.to_be_bytes());
    let shadow_fact: String = tx
        .query_row(
            "SELECT fact FROM temp.local_attention_stop_heads",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(shadow_fact, "[]");
    tx.commit().unwrap();
}

#[test]
fn missing_arrangement_indexes_refuse_without_scan_or_read_repair() {
    use crate::store::attention_stop_heads as stops;
    let store = Store::open_memory("alder").unwrap();
    let ns = namespace(&store, "fixture/human-missing-index");
    let writer = store.connection.write();
    stops::create_schema(&writer).unwrap();
    writer
        .execute_batch(
            "DROP INDEX main.attention_human_membership_run;
         DROP INDEX main.attention_stop_head_by_requester;",
        )
        .unwrap();
    assert!(selected(&writer, &ns, &["mission-run/missing".into()], 0).is_err());
    assert!(stops::maximum(&writer, &ns, "agent/asker").is_err());
    let indexes: usize = writer
        .query_row(
            "SELECT count(*) FROM main.sqlite_schema WHERE type='index'
         AND name IN ('attention_human_membership_run','attention_stop_head_by_requester')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(indexes, 0);
}

#[test]
fn canonical_stop_arrangement_tracks_rekeys_removals_rollback_and_isolated_namespaces() {
    use crate::store::attention_stop_heads as stops;
    let store = Store::open_memory("alder").unwrap();
    let first = namespace(&store, "fixture/human-stops-one");
    let second = namespace(&store, "fixture/human-stops-two");
    let mut writer = store.connection.write();
    stops::create_schema(&writer).unwrap();
    let mut inputs = BTreeMap::from([
        (
            "old",
            stops::Head {
                requester: "agent/asker".into(),
                key: (9, "writer-z".into(), 9, "b".into(), 4, "old".into()),
            },
        ),
        (
            "new",
            stops::Head {
                requester: "agent/asker".into(),
                key: (10, "writer-a".into(), 1, "a".into(), 1, "new".into()),
            },
        ),
        (
            "tie",
            stops::Head {
                requester: "agent/asker".into(),
                key: (10, "writer-b".into(), 1, "b".into(), 0, "tie".into()),
            },
        ),
    ]);
    let tx = writer.transaction().unwrap();
    for (claim, head) in &inputs {
        let changed = stops::replace(&tx, &first, claim, Some(head)).unwrap();
        assert_eq!(changed.writes, 1);
        assert_eq!(
            changed.affected_requesters,
            BTreeSet::from(["agent/asker".into()])
        );
    }
    let unchanged = stops::replace(&tx, &first, "new", inputs.get("new")).unwrap();
    assert_eq!(unchanged.writes, 0);
    assert!(unchanged.affected_requesters.is_empty());
    tx.commit().unwrap();
    fn assert_head(c: &Connection, ns: &Namespace, inputs: &BTreeMap<&str, stops::Head>) {
        let oracle = inputs
            .values()
            .filter(|head| head.requester == "agent/asker")
            .max_by(|left, right| left.key.cmp(&right.key))
            .cloned();
        assert_eq!(stops::maximum(c, ns, "agent/asker").unwrap(), oracle);
    }
    assert_head(&writer, &first, &inputs);
    assert!(
        stops::maximum(&writer, &second, "agent/asker")
            .unwrap()
            .is_none()
    );
    {
        let tx = writer.transaction().unwrap();
        let mut correction = inputs["new"].clone();
        correction.key.1 = "writer-z".into();
        stops::replace(&tx, &first, "new", Some(&correction)).unwrap();
        assert_eq!(
            stops::maximum(&tx, &first, "agent/asker").unwrap(),
            Some(correction)
        );
        // Rollback restores the original canonical maximum without replaying declarations.
    }
    assert_head(&writer, &first, &inputs);
    let tx = writer.transaction().unwrap();
    let mut moved = inputs["tie"].clone();
    moved.requester = "agent/other".into();
    let changed = stops::replace(&tx, &first, "tie", Some(&moved)).unwrap();
    assert_eq!(
        changed.affected_requesters,
        BTreeSet::from(["agent/asker".into(), "agent/other".into()])
    );
    inputs.insert("tie", moved);
    assert_head(&tx, &first, &inputs);
    let future = stops::Head {
        requester: "agent/asker".into(),
        key: (
            u128::MAX,
            "writer".into(),
            0,
            "b".into(),
            0,
            "future".into(),
        ),
    };
    stops::replace(&tx, &first, "future", Some(&future)).unwrap();
    inputs.insert("future", future);
    assert_head(&tx, &first, &inputs);
    assert_eq!(
        stops::replace(&tx, &first, "future", None).unwrap().writes,
        1
    );
    inputs.remove("future");
    assert_head(&tx, &first, &inputs);
    let plan: Vec<String> = tx.prepare("EXPLAIN QUERY PLAN SELECT claim FROM local_attention_stop_heads WHERE namespace=?1 AND requester=?2 ORDER BY canonical_key DESC,claim DESC LIMIT 1").unwrap()
        .query_map(params![first.as_str(),"agent/asker"], |r| r.get(3)).unwrap().collect::<rusqlite::Result<_>>().unwrap();
    assert!(
        plan.iter()
            .any(|line| line.contains("attention_stop_head_by_requester"))
    );
    assert!(
        plan.iter()
            .all(|line| !line.contains("TEMP B-TREE") && !line.starts_with("SCAN")),
        "{plan:?}"
    );
    tx.commit().unwrap();
    let invalid_claim = "x".repeat(4097);
    writer
        .execute(
            "INSERT INTO local_attention_stop_heads VALUES(?1,?2,'agent/asker',X'FF','[]')",
            params![first.as_str(), invalid_claim],
        )
        .unwrap();
    assert!(
        stops::maximum(&writer, &first, "agent/asker")
            .unwrap_err()
            .to_string()
            .contains("STOP maximum fact bound")
    );
    writer
        .execute(
            "DELETE FROM local_attention_stop_heads WHERE namespace=?1 AND claim=?2",
            params![first.as_str(), invalid_claim],
        )
        .unwrap();
    assert_head(&writer, &first, &inputs);
    let tx = writer.transaction().unwrap();
    let oversized = stops::Head {
        requester: "agent/asker".into(),
        key: (0, "x".repeat(4097), 0, "b".into(), 0, "oversized".into()),
    };
    assert!(stops::replace(&tx, &first, "oversized", Some(&oversized)).is_err());
    assert_head(&tx, &first, &inputs);
}

#[test]
fn native_planning_and_revision_runs_survive_public_clock_filter_and_retract_separately() {
    let (store, origin, _) = person_work::tests::fixture();
    let ns = namespace(&store, "fixture/human-approvals");
    let run = store.mission_run(&origin.run).unwrap().unwrap();
    let kdl = "version 2\nmission \"human-approval-fixture\" state=\"ready\" { goal \"Review an invented plan\" }";
    let intent = crate::graph::parse_test_intent(kdl, "alder").unwrap();
    let mission = store
        .mission(
            &intent,
            IntentInput {
                kdl: kdl.into(),
                source_name: None,
            },
        )
        .unwrap();
    let doc = store
        .put_document(
            "doc/human-approval-fixture/request",
            b"Review an invented plan",
            &None,
            "human-approval-request",
        )
        .unwrap();
    let reference = format!("{}@{}", doc.name, doc.hash);
    let captured = now_ms();
    store.set_write_clock_at(captured + 60_000).unwrap();
    let planning = "planning-session/fixture-human-approval";
    for (kind, fields) in [
        (
            "started",
            json!({"mission":"mission/human-approval-fixture","request":reference,"workspace":"/tmp","requester":"person/avery","planner":"agent/alder.asker","target_run":run.subject,"target_generation":run.generation}),
        ),
        (
            "candidate-submitted",
            json!({"candidate_revision":1,"markdown":reference,"kdl":reference,"mission_revision":"fixture-plan-one"}),
        ),
        (
            "previewed",
            json!({"candidate_revision":1,"preview_hash":"fixture-preview-one","store_index":mission.store_index,"graph":"fixture","diff":"new","mission":mission}),
        ),
    ] {
        append(
            &store,
            planning,
            &format!("planning-session.{kind}"),
            "person/avery",
            fields,
        );
    }
    let revision = "revision-proposal/fixture-human-approval";
    append(
        &store,
        revision,
        "revision-proposal.created",
        "person/avery",
        json!({
            "run":run.subject,"source_generation":run.generation,"candidate_revision":run.revision,
            "reason":"Review the invented revision","status":"pending-approval","cutover":"restart-active",
            "compatible_steps":[],"reviewers":["person/avery","person/operator"],"preview_hash":"fixture-revision-preview"
        }),
    );
    let raw = store.mission_run_attention_items(None).unwrap();
    assert!(
        raw.iter()
            .any(|item| item.kind == "launch-approval" && item.subject == planning)
    );
    assert_eq!(
        raw.iter()
            .filter(|item| item.kind == "revision-approval" && item.subject == revision)
            .count(),
        2
    );
    assert!(store.attention_snapshot(None, captured).unwrap().is_empty());
    assert_eq!(
        refresh(&store, &ns, captured),
        BTreeSet::from([run.subject.clone()])
    );
    {
        let c = store.readers.get();
        let selected = selected(&c, &ns, &[run.subject.clone()], captured).unwrap();
        assert!(selected[0].waiting);
        assert_eq!(
            selected[0].next_deadline, None,
            "approval membership has no common public clock filter"
        );
    }
    append(
        &store,
        planning,
        "planning-session.approved",
        "person/avery",
        json!({"mission_revision":"fixture-plan-one","requester":"person/avery"}),
    );
    assert_eq!(
        refresh(&store, &ns, captured),
        BTreeSet::from([run.subject.clone()])
    );
    append(
        &store,
        revision,
        "revision-proposal.cancelled",
        "person/avery",
        json!({"status":"cancelled","reason":"Question withdrawn"}),
    );
    assert!(refresh(&store, &ns, captured).is_empty());
    store.replay_replication_graph().unwrap();
    assert!(refresh(&store, &ns, captured).is_empty());
}

#[test]
fn installer_membership_values_preserve_the_entire_clock_domain() {
    let input = Input {
        family: Family::Person,
        source: "step-run/fixture-wide-clock".into(),
        runs: vec![Membership {
            run: "mission-run/fixture-wide-clock".into(),
            eligible: u128::MAX,
        }],
    };
    let value = serde_json::to_value(&input).unwrap();
    assert_eq!(value["runs"][0]["eligible"], u128::MAX.to_string());
    let restored: Input = serde_json::from_value(value).unwrap();
    assert_eq!(restored.runs, input.runs);
}

#[test]
fn owned_person_renderer_matches_native_ask_actor_rekey_and_ordinary_step() {
    use crate::store::attention_snapshot::{
        person_attention_item, person_attention_item_from_facts,
    };
    fn assert_row(store: &Store, subject: &str) {
        let (view, ask, activated, current, expected) = store
            .read_snapshot(|_| {
                let c = store.readers.get();
                let clock = now_ms();
                let view = person_work::step(&c, subject)?.unwrap();
                let ask = person_work::request(&c, subject)?;
                let current = if let Some(ask) = &ask {
                    person_work::current(&c, ask, clock)?
                } else {
                    person_work::run_live(&c, &view.run, Some(&view.generation), false)?
                };
                let activated: Option<String> = c.query_row(
                    "SELECT activated_at_unix_ms FROM step_runs WHERE subject=?1",
                    [subject],
                    |r| r.get(0),
                )?;
                let expected = person_attention_item(&c, subject, clock)?;
                Ok((
                    view,
                    ask,
                    activated.and_then(|value| value.parse().ok()),
                    current,
                    expected,
                ))
            })
            .unwrap();
        let actual = person_attention_item_from_facts(subject, view, ask, activated, current);
        assert_eq!(
            serde_json::to_value(actual).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
    }
    let (store, origin, request) = person_work::tests::fixture();
    let ask = store.ask_person(&request).unwrap();
    assert_row(&store, &ask.subject);
    store.connection.batched(|tx| -> Result<()> {
        tx.execute("UPDATE step_runs SET assignee='person/operator',goals='[\"Updated question\"]' WHERE subject=?1", [&ask.subject])?;
        Ok(())
    }).unwrap().unwrap();
    assert_row(&store, &ask.subject);
    store
        .set_step_state(&origin.subject, "failed", Some("retry"))
        .unwrap();
    store.retry_step(&origin.subject, "new attempt", 0).unwrap();
    assert_row(&store, &ask.subject);

    let (store, origin, _) = person_work::tests::fixture();
    store
        .set_step_state(&origin.subject, "completed", None)
        .unwrap();
    let run = store.mission_run(&origin.run).unwrap().unwrap();
    let review = run.steps.iter().find(|step| step.step == "review").unwrap();
    store
        .set_step_state(&review.subject, "ready", None)
        .unwrap();
    assert!(
        store
            .person_attention_items(None, now_ms())
            .unwrap()
            .iter()
            .any(|item| item.subject == review.subject)
    );
    assert_row(&store, &review.subject);
}
