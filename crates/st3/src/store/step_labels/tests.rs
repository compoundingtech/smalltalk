use super::*;

fn run(store: &Store) -> MissionRunView {
    let intent = crate::parse_intent(
        "version 2\nmission \"label-sample\" state=\"ready\" { goal \"Prepare samples.\"; step \"build\" { title \"Build sample\"; goal \"First goal\"; goal \"Second goal\"; }; step \"review\" { goal \"Review sample\"; } }\n",
        store.origin(),
    ).unwrap();
    store.apply_internal(&intent, "publish").unwrap();
    store
        .create_mission_run(&MissionRunRequest {
            mission: "label-sample".into(),
            revision: None,
            workspace: "/example/project".into(),
            requester: Some("person/avery".into()),
            mode: None,
            inputs: BTreeMap::new(),
            idempotency_key: "start".into(),
        })
        .unwrap()
}

#[test]
fn selected_labels_preserve_public_fields_and_indexed_batch() {
    let store = Store::open_memory("birch").unwrap();
    let run = run(&store);
    let subjects = vec![
        run.steps[0].subject.clone(),
        "step-run/absent".into(),
        run.steps[0].subject.clone(),
    ];
    let connection = store.readers.get();
    let labels = rows(&connection, &subjects).unwrap();
    assert_eq!(labels.len(), 1);
    let label = &labels[&run.steps[0].subject];
    assert_eq!(label.run, run.subject);
    assert_eq!(label.mission, "mission/label-sample");
    assert_eq!(label.path, "build");
    assert_eq!(label.title.as_deref(), Some("Build sample"));
    assert_eq!(label.goal.as_deref(), Some("First goal"));
    assert_eq!(label.status, run.steps[0].status);
    assert_eq!(label.updated_at_unix_ms, run.steps[0].updated_at_unix_ms);
    assert!(
        rows(
            &connection,
            &vec!["step-run/absent".into(); MAX_SELECTED + 1]
        )
        .is_err()
    );
    assert!(rows(&connection, &[]).unwrap().is_empty());
    let plan = connection
        .prepare(&format!("EXPLAIN QUERY PLAN {QUERY}"))
        .unwrap()
        .query_map([serde_json::to_string(&subjects).unwrap()], |row| {
            row.get::<_, String>(3)
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert!(
        plan.iter()
            .any(|line| line.contains("SEARCH s") && line.contains("subject=?")),
        "{plan:?}"
    );
    assert!(
        plan.iter()
            .any(|line| line.contains("SEARCH r") && line.contains("id=?")),
        "{plan:?}"
    );
    assert!(
        !plan
            .iter()
            .any(|line| line.contains("SCAN s") || line.contains("SCAN r")),
        "{plan:?}"
    );
}

#[test]
fn caller_snapshot_does_not_mix_labels_from_a_newer_source_cut() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(&directory.path().join("store.sqlite"), "birch").unwrap();
    let run = run(&store);
    let subjects = vec![run.steps[0].subject.clone()];
    let connection = store.readers.get();
    let snapshot = connection.unchecked_transaction().unwrap();
    let before = rows(&snapshot, &subjects).unwrap();
    store
        .set_step_state(&subjects[0], "completed", None)
        .unwrap();
    let old_cut = rows(&snapshot, &subjects).unwrap();
    assert_eq!(old_cut[&subjects[0]].status, before[&subjects[0]].status);
    assert_eq!(
        old_cut[&subjects[0]].updated_at_unix_ms,
        before[&subjects[0]].updated_at_unix_ms
    );
    assert_eq!(
        store.step_labels(&subjects).unwrap()[&subjects[0]].status,
        "completed"
    );
    snapshot.commit().unwrap();
    assert_eq!(
        rows(&connection, &subjects).unwrap()[&subjects[0]].status,
        "completed"
    );
}
