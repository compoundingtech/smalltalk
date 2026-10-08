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
