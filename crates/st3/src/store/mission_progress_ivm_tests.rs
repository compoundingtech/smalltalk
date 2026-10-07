use super::super::tests::{exchange_from, receive_and_project};
use super::*;
use smallclaims::ivm::{SourceCut, events};

fn register(store: &Store) -> Arc<Views> {
    let views = Arc::new(Views::new(definitions()).unwrap());
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    let index = current_index_tx(&tx).unwrap();
    views.create_schema(&tx).unwrap();
    views
        .initialize_empty(
            &tx,
            SourceCut {
                epoch: 1,
                admitted: index,
                projected: index,
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
fn publish(store: &Store) {
    let intent=crate::parse_intent("version 2\nmission \"orchard\" state=\"ready\" { concurrent-runs max=4; goal \"Prepare samples.\"; step \"build\" { goal \"Build a sample.\"; }; step \"review\" { goal \"Review a sample.\"; } }\n",store.origin()).unwrap();
    store.apply_internal(&intent, "publish").unwrap();
}
fn start(store: &Store, id: &str) -> MissionRunView {
    store
        .create_mission_run(&MissionRunRequest {
            mission: "orchard".into(),
            revision: None,
            workspace: "/example/project".into(),
            requester: Some("person/avery".into()),
            mode: None,
            inputs: BTreeMap::new(),
            idempotency_key: id.into(),
        })
        .unwrap()
}
// Test-only source certificate, after completed Store actions or replication projection.
// The production owner needs complete projected source coverage, not a largest-index read.
fn checkpoint(store: &Store, views: &Views) {
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    let index = current_index_tx(&tx).unwrap();
    let previous = source_cut(&tx).unwrap().unwrap();
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
    while flush(&tx, views, now_ms(), 2).unwrap() != 0 {}
    tx.commit().unwrap();
}
fn expected(connection: &Connection, runs: &[String]) -> BTreeMap<String, Progress> {
    runs.iter().filter_map(|run| {
        let id=run.trim_start_matches("mission-run/");
        let generation: Option<String>=connection.query_row("SELECT current_generation_id FROM mission_runs WHERE id=?1",[id],|row|row.get(0)).optional().unwrap();
        generation.map(|generation| {
            let (total,done)=connection.query_row("SELECT COUNT(*),COALESCE(SUM(status='completed'),0) FROM step_runs WHERE run_id=?1 AND generation_id=?2",params![id,generation],|row|Ok((row.get(0)?,row.get(1)?))).unwrap();
            (run.clone(),Progress {generation:format!("run-generation/{generation}"),total,done})
        })
    }).collect()
}
fn parity(store: &Store, views: &Views, runs: &[String]) {
    checkpoint(store, views);
    let connection = store.readers.get();
    assert_eq!(
        rows(&connection, views, runs).unwrap(),
        expected(&connection, runs)
    );
    let source=connection.prepare("SELECT run_id,generation_id,COUNT(*),COALESCE(SUM(status='completed'),0) FROM step_runs GROUP BY run_id,generation_id ORDER BY run_id,generation_id").unwrap()
        .query_map([],|row|Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,u64>(2)?,row.get::<_,u64>(3)?))).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap();
    let counts=connection.prepare("SELECT run_id,generation_id,SUM(count),SUM(CASE WHEN status='completed' THEN count ELSE 0 END) FROM local_progress_counts GROUP BY run_id,generation_id ORDER BY run_id,generation_id").unwrap()
        .query_map([],|row|Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,u64>(2)?,row.get::<_,u64>(3)?))).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap();
    assert_eq!(counts, source);
}
fn extra(connection: &Connection, template: &str, subject: &str) {
    connection.execute("INSERT INTO step_runs(subject,run_id,generation_id,step_path,definition_hash,status,attempt,assignee,available_to,agentless,title,goals,worker_reported,created_at_unix_ms,updated_at_unix_ms,readiness_epoch,constraints)
      SELECT ?1,run_id,generation_id,'sample-extra',definition_hash,'completed',attempt,assignee,available_to,agentless,title,goals,worker_reported,created_at_unix_ms,updated_at_unix_ms,readiness_epoch,constraints FROM step_runs WHERE subject=?2",params![subject,template]).unwrap();
}

#[test]
fn real_store_count_parity_and_selected_batches() {
    let (store, views) = fixture();
    publish(&store);
    let runs = (0..3)
        .map(|n| start(&store, &format!("start-{n}")))
        .collect::<Vec<_>>();
    let ids = runs
        .iter()
        .map(|run| run.subject.clone())
        .collect::<Vec<_>>();
    parity(&store, &views, &ids);
    for run in &runs {
        store
            .set_step_state(&run.steps[0].subject, "completed", None)
            .unwrap();
        parity(&store, &views, &ids);
    }
    let connection = store.readers.get();
    assert!(rows(&connection, &views, &vec![ids[0].clone(); 502]).is_err());
    assert!(
        rows(&connection, &views, &["mission-run/missing".into()])
            .unwrap()
            .is_empty()
    );
    for value in rows(&connection, &views, &ids).unwrap().values() {
        assert_eq!((value.total, value.done), (2, 1));
    }
}

#[test]
fn old_new_run_generation_membership_and_zero_step_runs() {
    let (store, views) = fixture();
    publish(&store);
    let first = start(&store, "first");
    let second = start(&store, "second");
    let ids = vec![first.subject.clone(), second.subject.clone()];
    parity(&store, &views, &ids);
    store.connection.write().execute("UPDATE step_runs SET subject='step-run/sample/moved',run_id=?1,generation_id=?2,step_path='sample-moved',status='completed' WHERE subject=?3",params![second.id,generation_id_from_subject(&second.generation),first.steps[0].subject]).unwrap();
    assert!(rows(&store.readers.get(), &views, &ids).is_err());
    parity(&store, &views, &ids);
    let values = rows(&store.readers.get(), &views, &ids).unwrap();
    assert_eq!(
        (values[&first.subject].total, values[&second.subject].total),
        (1, 3)
    );
    store
        .connection
        .write()
        .execute(
            "UPDATE mission_runs SET current_generation_id=?1 WHERE id=?2",
            params![generation_id_from_subject(&first.generation), second.id],
        )
        .unwrap();
    parity(&store, &views, &ids);
    assert_eq!(
        rows(&store.readers.get(), &views, &ids).unwrap()[&second.subject].total,
        0
    );
    store
        .connection
        .write()
        .execute("DELETE FROM step_runs WHERE run_id=?1", [first.id])
        .unwrap();
    parity(&store, &views, &ids);
    assert_eq!(
        rows(&store.readers.get(), &views, &ids).unwrap()[&first.subject].total,
        0
    );
}

#[test]
fn bounded_flush_and_rollback_preserve_source_output_and_tokens() {
    let (store, views) = fixture();
    publish(&store);
    let runs = (0..3)
        .map(|n| start(&store, &format!("start-{n}")))
        .collect::<Vec<_>>();
    let ids = runs
        .iter()
        .map(|run| run.subject.clone())
        .collect::<Vec<_>>();
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    let previous = source_cut(&tx).unwrap().unwrap();
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
    assert_eq!(flush(&tx, &views, now_ms(), 1).unwrap(), 1);
    assert!(rows(&tx, &views, &ids).is_err());
    tx.commit().unwrap();
    drop(writer);
    parity(&store, &views, &ids);
    let before = rows(&store.readers.get(), &views, &ids).unwrap();
    let token = views.readiness(&store.readers.get(), VIEW, 1).unwrap();
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    tx.execute(
        "UPDATE step_runs SET status='completed' WHERE subject=?1",
        [&runs[0].steps[0].subject],
    )
    .unwrap();
    assert_eq!(flush(&tx, &views, now_ms(), 1).unwrap(), 1);
    assert_ne!(rows(&tx, &views, &ids).unwrap(), before);
    tx.rollback().unwrap();
    drop(writer);
    assert_eq!(rows(&store.readers.get(), &views, &ids).unwrap(), before);
    assert_eq!(
        views.readiness(&store.readers.get(), VIEW, 1).unwrap(),
        token
    );
    parity(&store, &views, &ids);
}

#[test]
fn non_completion_status_and_identical_updates_write_no_count_or_output() {
    let (store, views) = fixture();
    publish(&store);
    let run = start(&store, "start");
    let ids = vec![run.subject.clone()];
    parity(&store, &views, &ids);
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    tx.execute_batch("CREATE TEMP TABLE count_writes(kind TEXT); CREATE TEMP TRIGGER count_insert AFTER INSERT ON local_progress_counts BEGIN INSERT INTO count_writes VALUES('count'); END; CREATE TEMP TRIGGER count_update AFTER UPDATE ON local_progress_counts BEGIN INSERT INTO count_writes VALUES('count'); END; CREATE TEMP TRIGGER count_delete AFTER DELETE ON local_progress_counts BEGIN INSERT INTO count_writes VALUES('count'); END; CREATE TEMP TRIGGER progress_update AFTER UPDATE ON local_progress_rows BEGIN INSERT INTO count_writes VALUES('output'); END;").unwrap();
    for status in [
        "working", "claimed", "ready", "blocked", "pending", "pending",
    ] {
        tx.execute(
            "UPDATE step_runs SET status=?1 WHERE subject=?2",
            params![status, run.steps[0].subject],
        )
        .unwrap();
    }
    tx.execute(
        "UPDATE step_runs SET title='Updated title' WHERE subject=?1",
        [&run.steps[0].subject],
    )
    .unwrap();
    assert_eq!(flush(&tx, &views, now_ms(), 10).unwrap(), 0);
    assert_eq!(
        tx.query_row("SELECT COUNT(*) FROM count_writes", [], |row| row
            .get::<_, u64>(0))
            .unwrap(),
        0
    );
}

#[test]
fn explicit_populated_capture_resumes_and_tracks_changes_before_cursor() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sample.sqlite3");
    let store = Store::open(&path, "birch").unwrap();
    publish(&store);
    let runs = (0..3)
        .map(|n| start(&store, &format!("start-{n}")))
        .collect::<Vec<_>>();
    let ids = runs
        .iter()
        .map(|run| run.subject.clone())
        .collect::<Vec<_>>();
    store
        .connection
        .write()
        .execute("DELETE FROM step_runs WHERE run_id=?1", [&runs[2].id])
        .unwrap();
    let views = register(&store);
    assert_eq!(
        views.readiness(&store.readers.get(), VIEW, 1).unwrap(),
        Readiness::Fenced
    );
    assert!(rows(&store.readers.get(), &views, &ids).is_err());
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    assert_eq!(seed_page(&tx, 1).unwrap(), (1, false));
    tx.commit().unwrap();
    drop(writer);
    extra(
        &store.connection.write(),
        &runs[0].steps[0].subject,
        "step-run/000-sample/early",
    );
    store
        .connection
        .write()
        .execute(
            "UPDATE step_runs SET status='completed' WHERE subject=?1",
            [&runs[1].steps[0].subject],
        )
        .unwrap();
    drop(store);
    let reopened = Store::open(&path, "birch").unwrap();
    let mut pages = 0;
    loop {
        let mut writer = reopened.connection.write();
        let tx = writer.transaction().unwrap();
        let (processed, complete) = seed_page(&tx, 1).unwrap();
        assert!(processed <= 1);
        tx.commit().unwrap();
        pages += 1;
        assert!(pages < 20);
        if complete {
            break;
        }
    }
    let mut writer = reopened.connection.write();
    let tx = writer.transaction().unwrap();
    while backfill_page(&tx, &views, 1).unwrap() != 0 {}
    assert!(clean(&tx).unwrap());
    assert!(
        rows(&tx, &views, &ids).is_err(),
        "backfill does not publish readiness"
    );
    // Explicit fixture owner attestation only: capture and all outputs are complete. A real
    // installer must independently certify source/projection/canonical/checkpoint coverage.
    tx.execute("UPDATE ivm_views SET ready=1 WHERE name=?1", [VIEW])
        .unwrap();
    assert_eq!(rows(&tx, &views, &ids).unwrap(), expected(&tx, &ids));
    assert_eq!(rows(&tx, &views, &ids).unwrap()[&runs[2].subject].total, 0);
    assert!(backfill_page(&tx, &views, 1).is_err());
    tx.commit().unwrap();
    drop(writer);
    parity(&reopened, &views, &ids);
}

#[test]
fn source_pending_is_not_a_fenced_backfill_permission() {
    let (store, views) = fixture();
    publish(&store);
    let run = start(&store, "start");
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    assert_eq!(
        views.readiness(&tx, VIEW, 1).unwrap(),
        Readiness::SourcePending
    );
    assert!(backfill_page(&tx, &views, 1).is_err());
    assert!(!clean(&tx).unwrap());
    tx.rollback().unwrap();
    drop(writer);
    parity(&store, &views, &[run.subject]);
}

#[test]
fn permuted_replication_duplicates_and_retirement_match_raw_progress() {
    let source = Store::open_memory("cedar").unwrap();
    publish(&source);
    let run = start(&source, "start");
    source
        .set_step_state(&run.steps[0].subject, "completed", None)
        .unwrap();
    let exchange = exchange_from(&source, &ReplicationInventory::default());
    for reverse in [false, true] {
        let (target, views) = fixture();
        let mut permuted = exchange.clone();
        if reverse {
            permuted.envelopes.reverse();
        }
        receive_and_project(&target, "cedar", &permuted);
        parity(&target, &views, &[run.subject.clone()]);
        receive_and_project(&target, "cedar", &permuted);
        parity(&target, &views, &[run.subject.clone()]);
        assert_eq!(
            rows(&target.readers.get(), &views, &[run.subject.clone()]).unwrap()[&run.subject].done,
            1
        );
    }
    let (store, views) = fixture();
    publish(&store);
    let run = start(&store, "start");
    parity(&store, &views, &[run.subject.clone()]);
    store
        .request_mission_run_cancellation(&run.subject, "invented fixture cleanup")
        .unwrap();
    store
        .set_mission_run_state(&run.id, "cancelled", "terminal", None)
        .unwrap();
    store
        .retire_mission("mission/orchard", "person/avery", "retire")
        .unwrap();
    parity(&store, &views, &[run.subject]);
}
