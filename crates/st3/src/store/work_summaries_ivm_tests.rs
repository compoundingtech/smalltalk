use super::super::tests::{exchange_from, receive_and_project};
use super::*;
use smallclaims::ivm::{SourceCut, events};

fn register(store: &Store) -> Arc<Views> {
    let views = Arc::new(Views::new(definitions()).unwrap());
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    assert_eq!(current_index_tx(&tx).unwrap(), 0);
    views.create_schema(&tx).unwrap();
    views
        .initialize_empty(
            &tx,
            SourceCut {
                epoch: 1,
                admitted: 0,
                projected: 0,
                local_generation: 0,
            },
        )
        .unwrap();
    events::install(&tx, 1024).unwrap();
    tx.commit().unwrap();
    views
}
fn fixture() -> (Store, Arc<Views>) {
    let store = Store::open_memory("birch").unwrap();
    let views = register(&store);
    (store, views)
}
fn mission(store: &Store) -> MissionRunView {
    let intent=crate::parse_intent("version 2\nmission \"orchard\" state=\"ready\" { goal \"Prepare samples.\"; step \"build\" { goal \"Build a sample.\"; }; step \"review\" { goal \"Review a sample.\"; } }\n",store.origin()).unwrap();
    store.apply_internal(&intent, "publish").unwrap();
    store
        .create_mission_run(&MissionRunRequest {
            mission: "orchard".into(),
            revision: None,
            workspace: "/example/project".into(),
            requester: Some("person/avery".into()),
            mode: None,
            inputs: BTreeMap::new(),
            idempotency_key: "start".into(),
        })
        .unwrap()
}
fn append(store: &Store, subject: &str, kind: &str, attempt: u32, summary: &str) -> ClaimRecord {
    store
        .append_claim(&ClaimInput {
            subject: subject.into(),
            kind: kind.into(),
            actor: Some("agent/sample/worker".into()),
            fields: BTreeMap::from([
                ("attempt".into(), json!(attempt)),
                ("summary".into(), json!(summary)),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap()
}
// The batched writer runs on its own thread. A controlled accepted timestamp must be
// supplied on this transaction's thread instead of assuming that thread inherits the clock.
fn append_at(store: &Store, subject: &str, at: u128, summary: &str) -> ClaimRecord {
    let _reset = freeze(at);
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    let claim = append_claim_tx(
        &tx,
        store.origin(),
        subject,
        "work.progress",
        Some("agent/sample/worker"),
        &json!({"fields":{"attempt":1,"summary":summary}}),
        &[],
        None,
    )
    .unwrap();
    assert_eq!(claim.accepted_at_unix_ms, at);
    tx.commit().unwrap();
    claim
}
// Test-only complete capture after a real Store action or a completed replication pass.
// Production must capture every admitted source transaction and certify the contiguous
// projection frontier; this helper is not a production MAX-index adapter.
fn capture(store: &Store, views: &Views) {
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    let previous = source_cut(&tx).unwrap().unwrap();
    let records=tx.prepare("SELECT id,store_index,batch_id,subject,kind,origin,actor,body,predecessors,accepted_at_unix_ms FROM claims WHERE store_index>?1 ORDER BY store_index")
        .unwrap().query_map([previous.projected],claim_from_row).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap();
    for claim in records {
        if views.reads_kind(&claim.kind) {
            let canonical = smallclaims::store::canonical::claim_key(&tx, &claim.id).unwrap();
            let changes = views
                .change(&tx, None, Some((&claim, &canonical)), previous.epoch)
                .unwrap();
            assert!(changes.deferred.is_empty());
        }
    }
    let index = current_index_tx(&tx).unwrap();
    views
        .publish_cut(
            &tx,
            SourceCut {
                admitted: index,
                projected: index,
                ..previous
            },
        )
        .unwrap();
    tx.commit().unwrap();
}
fn advance(store: &Store, views: &Views, at: u128, limit: usize) -> usize {
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    let count = clock_page(&tx, views, at, limit).unwrap();
    tx.commit().unwrap();
    count
}
fn parity(store: &Store, views: &Views, steps: &[StepRunView], at: u128) {
    capture(store, views);
    while advance(store, views, at, 2) != 0 {}
    store
        .read_snapshot(|_| {
            let connection = store.readers.get();
            let selected = steps
                .iter()
                .map(|step| (step.subject.clone(), step.attempt))
                .collect::<Vec<_>>();
            let batch = rows(&connection, views, &selected, at)?;
            for step in steps {
                let mut expected = step.clone();
                enrich_step_summaries_at(&connection, &mut expected, at)?;
                let expected = Summaries {
                    progress_summary: expected.progress_summary,
                    progress_at_unix_ms: expected.progress_at_unix_ms,
                    completion_summary: expected.completion_summary,
                };
                assert_eq!(
                    row(&connection, views, &step.subject, step.attempt, at)?,
                    expected
                );
                assert_eq!(batch[&(step.subject.clone(), step.attempt)], expected);
            }
            Ok(())
        })
        .unwrap();
}
struct ResetClock;
impl Drop for ResetClock {
    fn drop(&mut self) {
        smallclaims::store::set_thread_clock(None);
    }
}
fn freeze(at: u128) -> ResetClock {
    smallclaims::store::set_thread_clock(Some(at));
    ResetClock
}
// Engine-level records make canonical ties/retraction independent of writer allocation.
fn synthetic(id: &str, subject: &str, kind: &str, at: u128, fields: Value) -> ClaimRecord {
    ClaimRecord {
        id: id.into(),
        store_index: 0,
        batch_id: "sample-batch".into(),
        subject: subject.into(),
        kind: kind.into(),
        origin: "cedar".into(),
        actor: None,
        operation_id: None,
        request_digest: None,
        body: json!({"fields":fields}),
        predecessors: Vec::new(),
        accepted_at_unix_ms: at,
    }
}
fn change(
    tx: &Transaction<'_>,
    views: &Views,
    old: Option<&ClaimRecord>,
    new: Option<&ClaimRecord>,
    writer: &str,
    sequence: u64,
    position: u64,
) -> smallclaims::ivm::Changes {
    let canonical = new.map(|claim| {
        smallclaims::store::canonical::key_from_record(claim, writer.into(), sequence, position)
    });
    let changes = views
        .change(tx, old, new.zip(canonical.as_ref()), 1)
        .unwrap();
    assert!(changes.deferred.is_empty());
    changes
}

#[test]
fn real_store_summary_parity_attempts_and_batched_reads() {
    let _clock = clock_snapshot();
    let (store, views) = fixture();
    let run = mission(&store);
    let subject = &run.steps[0].subject;
    for (attempt, kind, summary) in [
        (1, "work.progress", "first"),
        (1, "work.progress", "  latest progress \n"),
        (1, "work.submitted", " complete "),
        (2, "work.progress", "next attempt"),
        (1, "work.progress", " \t "),
    ] {
        append(&store, subject, kind, attempt, summary);
    }
    let mut steps = run.steps.clone();
    let mut second = steps[0].clone();
    second.attempt = 2;
    steps.push(second);
    parity(&store, &views, &steps, now_ms() + 10_000);
    let connection = store.readers.get();
    let value = row(&connection, &views, subject, 1, now_ms() + 10_000).unwrap();
    assert_eq!(value.progress_summary.as_deref(), Some("latest progress"));
    assert_eq!(value.completion_summary.as_deref(), Some("complete"));
    assert!(
        rows(
            &connection,
            &views,
            &vec![(subject.clone(), 1); 502],
            now_ms() + 10_000
        )
        .is_err()
    );
    assert_eq!(
        row(&connection, &views, subject, 999, now_ms() + 10_000).unwrap(),
        Summaries::default()
    );
}

#[test]
fn future_claims_require_bounded_captured_time_pages() {
    let at = 1_800_000_000_000;
    let _reset = freeze(at);
    let (store, views) = fixture();
    let run = mission(&store);
    for step in &run.steps {
        append_at(&store, &step.subject, at, "current");
    }
    parity(&store, &views, &run.steps, at);
    for step in &run.steps {
        append_at(&store, &step.subject, at + 100, "future");
    }
    capture(&store, &views);
    assert_eq!(
        next_deadline(&store.readers.get(), &views, at).unwrap(),
        Some(at + 100)
    );
    parity(&store, &views, &run.steps, at + 99);
    assert!(
        row(
            &store.readers.get(),
            &views,
            &run.steps[0].subject,
            1,
            at + 100
        )
        .is_err()
    );
    assert_eq!(advance(&store, &views, at + 100, 1), 1);
    assert!(
        row(
            &store.readers.get(),
            &views,
            &run.steps[0].subject,
            1,
            at + 100
        )
        .is_err(),
        "partial time coverage never ready"
    );
    assert_eq!(advance(&store, &views, at + 100, 1), 1);
    parity(&store, &views, &run.steps, at + 100);
    assert_eq!(
        row(
            &store.readers.get(),
            &views,
            &run.steps[0].subject,
            1,
            at + 100
        )
        .unwrap()
        .progress_summary
        .as_deref(),
        Some("future")
    );
    assert!(
        row(
            &store.readers.get(),
            &views,
            &run.steps[0].subject,
            1,
            at + 99
        )
        .is_err(),
        "no historical fallback"
    );
    assert_eq!(
        next_deadline(&store.readers.get(), &views, at + 100).unwrap(),
        None
    );
}

#[test]
fn canonical_ties_arrival_permutations_duplicates_and_retraction() {
    let records = [
        synthetic(
            "alpha",
            "step-run/sample/build",
            "work.progress",
            10,
            json!({"attempt":1,"summary":"first"}),
        ),
        synthetic(
            "beta",
            "step-run/sample/build",
            "work.progress",
            10,
            json!({"attempt":1,"summary":"writer winner"}),
        ),
        synthetic(
            "gamma",
            "step-run/sample/build",
            "work.progress",
            10,
            json!({"attempt":1,"summary":"position winner"}),
        ),
    ];
    for order in [[0, 1, 2], [2, 0, 1], [1, 2, 0]] {
        let (store, views) = fixture();
        advance(&store, &views, 10, 10);
        let mut writer = store.connection.write();
        let tx = writer.transaction().unwrap();
        for index in order {
            let (writer, position) = if index == 0 {
                ("birch", 9)
            } else {
                ("cedar", index as u64)
            };
            change(
                &tx,
                &views,
                None,
                Some(&records[index]),
                writer,
                1,
                position,
            );
        }
        assert_eq!(
            row(&tx, &views, &records[0].subject, 1, 10)
                .unwrap()
                .progress_summary
                .as_deref(),
            Some("position winner")
        );
        assert!(
            change(&tx, &views, None, Some(&records[2]), "cedar", 1, 2)
                .changed
                .is_empty()
        );
        change(&tx, &views, Some(&records[2]), None, "", 0, 0);
        assert_eq!(
            row(&tx, &views, &records[0].subject, 1, 10)
                .unwrap()
                .progress_summary
                .as_deref(),
            Some("writer winner")
        );
        change(&tx, &views, Some(&records[1]), None, "", 0, 0);
        assert_eq!(
            row(&tx, &views, &records[0].subject, 1, 10)
                .unwrap()
                .progress_summary
                .as_deref(),
            Some("first")
        );
        change(&tx, &views, Some(&records[0]), None, "", 0, 0);
        assert_eq!(
            row(&tx, &views, &records[0].subject, 1, 10).unwrap(),
            Summaries::default()
        );
    }
}

#[test]
fn sparse_legacy_handoff_and_old_new_subject_attempt_keys() {
    let (store, views) = fixture();
    advance(&store, &views, 20, 10);
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    let mut legacy = synthetic(
        "legacy",
        "step-run/sample/build",
        "work.progress",
        10,
        json!({"attempt":1,"summary":" legacy "}),
    );
    legacy.body = legacy.body["fields"].clone();
    change(&tx, &views, None, Some(&legacy), "birch", 1, 0);
    for (index, fields) in [
        json!({"attempt":1,"summary":"handoff","handoff_acknowledged":null}),
        json!({"attempt":1,"summary":false}),
        json!({"summary":"missing attempt"}),
        json!({"attempt":4294967296u64,"summary":"wide attempt"}),
        json!({"attempt":1,"summary":"   "}),
        json!({"attempt":1.5,"summary":"fractional"}),
    ]
    .into_iter()
    .enumerate()
    {
        let invalid = synthetic(
            &format!("invalid-{index}"),
            &legacy.subject,
            "work.progress",
            20,
            fields,
        );
        assert!(
            change(&tx, &views, None, Some(&invalid), "cedar", 1, index as u64)
                .changed
                .is_empty()
        );
    }
    assert_eq!(
        row(&tx, &views, &legacy.subject, 1, 20)
            .unwrap()
            .progress_summary
            .as_deref(),
        Some("legacy")
    );
    let mut moved = legacy.clone();
    moved.subject = "step-run/sample/review".into();
    moved.body = json!({"fields":{"attempt":2,"summary":"new owner"}});
    assert_eq!(
        change(&tx, &views, Some(&legacy), Some(&moved), "birch", 1, 0)
            .changed
            .len(),
        2
    );
    assert_eq!(
        row(&tx, &views, &legacy.subject, 1, 20).unwrap(),
        Summaries::default()
    );
    assert_eq!(
        row(&tx, &views, &moved.subject, 2, 20)
            .unwrap()
            .progress_summary
            .as_deref(),
        Some("new owner")
    );
}

#[test]
fn irrelevant_older_and_identical_claims_do_not_rewrite_public_rows() {
    let (store, views) = fixture();
    advance(&store, &views, 20, 10);
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    let latest = synthetic(
        "latest",
        "step-run/sample/build",
        "work.progress",
        20,
        json!({"attempt":1,"summary":"current"}),
    );
    change(&tx, &views, None, Some(&latest), "cedar", 1, 0);
    tx.execute_batch("CREATE TEMP TABLE summary_writes(key TEXT); CREATE TEMP TRIGGER summary_insert AFTER INSERT ON local_work_summaries BEGIN INSERT INTO summary_writes VALUES(NEW.key); END; CREATE TEMP TRIGGER summary_update AFTER UPDATE ON local_work_summaries BEGIN INSERT INTO summary_writes VALUES(NEW.key); END; CREATE TEMP TRIGGER summary_delete AFTER DELETE ON local_work_summaries BEGIN INSERT INTO summary_writes VALUES(OLD.key); END;").unwrap();
    let older = synthetic(
        "older",
        &latest.subject,
        "work.progress",
        10,
        json!({"attempt":1,"summary":"old"}),
    );
    let unrelated = synthetic(
        "other",
        &latest.subject,
        "work.renewed",
        20,
        json!({"attempt":1,"summary":"irrelevant"}),
    );
    for claim in [&older, &latest, &unrelated] {
        assert!(
            change(&tx, &views, None, Some(claim), "cedar", 1, 0)
                .changed
                .is_empty()
        );
    }
    assert_eq!(
        tx.query_row("SELECT COUNT(*) FROM summary_writes", [], |r| r
            .get::<_, u64>(0))
            .unwrap(),
        0
    );
}

#[test]
fn signed_replication_permutations_and_duplicates_match_canonical_reader() {
    use crate::fleet::{MemberKey, envelope_signature_message, verify_signature};
    const FLEET: &str = "018f6f0d-4a5d-7b8c-9d0e-123456789abc";
    let at = 1_800_000_000_000;
    let _reset = freeze(at);
    let source = Store::open_memory("cedar").unwrap();
    source.bind_fleet(FLEET).unwrap();
    let member = Arc::new(MemberKey::generate().unwrap().0);
    source.set_member_key(Some(member.clone())).unwrap();
    let run = mission(&source);
    for step in &run.steps {
        for (kind, text) in [
            ("work.progress", "first"),
            ("work.progress", "latest"),
            ("work.submitted", "complete"),
        ] {
            append(&source, &step.subject, kind, 1, text);
        }
    }
    let exchange = exchange_from(&source, &ReplicationInventory::default());
    assert!(exchange.envelopes.len() > 3);
    for envelope in &exchange.envelopes {
        assert_eq!(envelope.member_key.as_deref(), Some(member.public()));
        assert!(verify_signature(
            member.public(),
            &envelope_signature_message(FLEET, "cedar", envelope.sequence, &envelope.hash),
            envelope.signature.as_deref().unwrap()
        ));
    }
    for reverse in [false, true] {
        let (target, views) = fixture();
        let mut permuted = exchange.clone();
        if reverse {
            permuted.envelopes.reverse();
        }
        receive_and_project(&target, "cedar", &permuted);
        parity(&target, &views, &run.steps, at);
        receive_and_project(&target, "cedar", &permuted);
        parity(&target, &views, &run.steps, at);
        assert_eq!(
            row(&target.readers.get(), &views, &run.steps[0].subject, 1, at)
                .unwrap()
                .completion_summary
                .as_deref(),
            Some("complete")
        );
    }
}

#[test]
fn clock_and_summary_changes_roll_back_together() {
    let (store, views) = fixture();
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    let claim = synthetic(
        "future",
        "step-run/sample/build",
        "work.progress",
        10,
        json!({"attempt":1,"summary":"future"}),
    );
    change(&tx, &views, None, Some(&claim), "cedar", 1, 0);
    tx.commit().unwrap();
    let tx = writer.transaction().unwrap();
    assert_eq!(clock_page(&tx, &views, 10, 1).unwrap(), 1);
    assert_eq!(
        row(&tx, &views, &claim.subject, 1, 10)
            .unwrap()
            .progress_summary
            .as_deref(),
        Some("future")
    );
    tx.rollback().unwrap();
    drop(writer);
    let connection = store.readers.get();
    assert_eq!(clock(&connection).unwrap(), 0);
    assert_eq!(
        row(&connection, &views, &claim.subject, 1, 0).unwrap(),
        Summaries::default()
    );
    assert!(row(&connection, &views, &claim.subject, 1, 10).is_err());
}

#[test]
fn full_u128_clock_and_real_maintenance_query_plans() {
    let (store, views) = fixture();
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    let claim = synthetic(
        "maximum",
        "step-run/sample/build",
        "work.progress",
        u128::MAX,
        json!({"attempt":1,"summary":"maximum"}),
    );
    change(&tx, &views, None, Some(&claim), "cedar", 1, 0);
    assert_eq!(next_deadline(&tx, &views, 0).unwrap(), Some(u128::MAX));
    clock_page(&tx, &views, u128::MAX, 1).unwrap();
    assert_eq!(
        row(&tx, &views, &claim.subject, 1, u128::MAX)
            .unwrap()
            .progress_at_unix_ms,
        Some(u128::MAX)
    );
    assert_eq!(next_deadline(&tx, &views, u128::MAX).unwrap(), None);
    for query in [WINNER, NEXT] {
        let sql = format!("EXPLAIN QUERY PLAN {query}");
        let plan = tx
            .prepare(&sql)
            .unwrap()
            .query_map(
                params![
                    VIEW,
                    key(&claim.subject, 1).unwrap(),
                    "progress",
                    u128::MAX.to_be_bytes().as_slice()
                ],
                |r| r.get::<_, String>(3),
            )
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
            .join(" ");
        assert!(plan.contains("ivm_contributions_rank"), "{plan}");
        assert!(!plan.contains("TEMP B-TREE"), "{plan}");
    }
    let sql = format!("EXPLAIN QUERY PLAN {DUE}");
    let plan = tx
        .prepare(&sql)
        .unwrap()
        .query_map(params![u128::MAX.to_be_bytes().as_slice(), 1], |r| {
            r.get::<_, String>(3)
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
        .join(" ");
    assert!(plan.contains("local_work_summaries_deadline"), "{plan}");
    assert!(!plan.contains("TEMP B-TREE"), "{plan}");
    assert!(clock_page(&tx, &views, u128::MAX, 0).is_err());
}

#[test]
fn repaired_originals_remain_eligible_at_their_captured_time() {
    let (store, views) = fixture();
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    let original = synthetic(
        "original",
        "step-run/sample/build",
        "work.submitted",
        30,
        json!({"attempt":1,"summary":"original"}),
    );
    let replacement = synthetic(
        "replacement",
        &original.subject,
        "work.submitted",
        20,
        json!({"attempt":1,"summary":"replacement"}),
    );
    change(&tx, &views, None, Some(&original), "cedar", 1, 0);
    let canonical =
        smallclaims::store::canonical::key_from_record(&replacement, "birch".into(), 1, 0);
    assert!(
        views
            .repair(&tx, &original, (&replacement, &canonical), 1)
            .unwrap()
            .deferred
            .is_empty()
    );
    clock_page(&tx, &views, 20, 1).unwrap();
    assert_eq!(
        row(&tx, &views, &original.subject, 1, 20)
            .unwrap()
            .completion_summary
            .as_deref(),
        Some("replacement")
    );
    clock_page(&tx, &views, 30, 1).unwrap();
    assert_eq!(
        row(&tx, &views, &original.subject, 1, 30)
            .unwrap()
            .completion_summary
            .as_deref(),
        Some("original")
    );
}

#[test]
fn reopen_preserves_rows_pending_appends_and_source_edits_fence() {
    let _clock = clock_snapshot();
    let at = now_ms() + 10_000;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sample.sqlite3");
    let store = Store::open(&path, "birch").unwrap();
    let views = register(&store);
    let run = mission(&store);
    let subject = &run.steps[0].subject;
    append(&store, subject, "work.progress", 1, "before reopen");
    parity(&store, &views, &run.steps, at);
    let before = row(&store.readers.get(), &views, subject, 1, at).unwrap();
    let token = views.readiness(&store.readers.get(), VIEW, 1).unwrap();
    drop(store);
    let reopened = Store::open(&path, "birch").unwrap();
    assert_eq!(
        row(&reopened.readers.get(), &views, subject, 1, at).unwrap(),
        before
    );
    assert_eq!(
        views.readiness(&reopened.readers.get(), VIEW, 1).unwrap(),
        token
    );
    let claim = append(&reopened, subject, "work.progress", 1, "after reopen");
    assert!(
        row(&reopened.readers.get(), &views, subject, 1, at).is_err(),
        "uncaptured write stays pending"
    );
    parity(&reopened, &views, &run.steps, at);
    reopened
        .connection
        .write()
        .execute("UPDATE claims SET body='{}' WHERE id=?1", [claim.id])
        .unwrap();
    assert_eq!(
        views.readiness(&reopened.readers.get(), VIEW, 1).unwrap(),
        Readiness::Fenced
    );
    assert!(row(&reopened.readers.get(), &views, subject, 1, at).is_err());
    let mut writer = reopened.connection.write();
    let tx = writer.transaction().unwrap();
    assert!(clock_page(&tx, &views, at, 1).is_err());
}
