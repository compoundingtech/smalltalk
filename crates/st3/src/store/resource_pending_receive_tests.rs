// Real receipt/admission/open paths on finite synthetic stores. Not incident attribution.
#[test]
fn marked_pending_triggers_admit_observer_batches_and_revalidate_after_reopen() {
    let source = Store::open_memory("node").unwrap();
    let intent = parse_intent(
        r#"version 2
resource "github/acme/demo/pull/1" { kind "vcs.pull-request" }
observer "github/acme/demo/pull/1" {
 resource "resource/github/acme/demo/pull/1"
 provider "github.pull-request"
 locator "acme/demo#1"
 field "state"
}
"#,
        "node",
    )
    .unwrap();
    source
        .apply_internal(&intent, "pending-receive-declaration")
        .unwrap();
    let observer = "observer/github/acme/demo/pull/1";
    let resource = "resource/github/acme/demo/pull/1";
    let revision = source.selected_desired_revision(observer).unwrap().unwrap();
    for (index, state) in ["open", "closed", "open"].iter().enumerate() {
        source
            .record_resource_observation(
                observer,
                &revision,
                Some(&format!("attempt-{index}")),
                resource,
                Some(&format!("cursor-{index}")),
                &json!({"state":state}),
                100 + index as u128,
                &[],
                None,
            )
            .unwrap();
    }
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("pending-receive.sqlite3");
    let target = Store::open(&path, "peer").unwrap();
    target.bind_fleet(TEST_FLEET).unwrap();
    let wire = exchange_from(&source, &target.replication_inventory().unwrap());
    assert!(wire.envelopes.len() <= 16);
    let check_triggers = |store: &Store| {
        let reader = store.readers.get();
        let marker: String = reader
            .query_row(
                "SELECT value FROM meta WHERE key='resource_pending_trigger_version'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(marker, "1");
        for name in [
            "resource_observations_insert",
            "resource_observations_delete",
            "resource_observations_record_insert",
            "resource_observations_record_update",
        ] {
            let sql: String = reader
                .query_row(
                    "SELECT sql FROM sqlite_schema WHERE type='trigger' AND name=?1",
                    [name],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(sql.contains("NOT EXISTS"), "{name}: {sql}");
        }
    };
    check_triggers(&target);
    target
        .receive_replication_exchange("node", TEST_FLEET, &wire)
        .unwrap();
    // Existing pending work is the duplicate-key condition that the real triggers must tolerate.
    target
        .connection
        .write()
        .execute(
            /* sqlite-large-value: allow-for-now lv-h60e7d65ec813 owner=st3::store::resource_pending_receive_tests TODO=bound-or-reference-values-and-reclamation */ "INSERT INTO local_resource_projection_pending(subject) VALUES(?1)",
            [resource],
        )
        .unwrap();
    target.validate_replication_backlog().unwrap();
    assert!(target.replica_records(true).unwrap().is_empty());
    assert!(target.project_replication_backlog().unwrap());
    assert_eq!(
        target.latest_actual_value(resource).unwrap().unwrap()["facts"]["state"],
        "open"
    );
    let records = target.replica_records(false).unwrap();
    assert!(
        records
            .iter()
            .any(|record| record.subject.as_deref() == Some(observer))
    );
    assert!(
        records
            .iter()
            .any(|record| record.subject.as_deref() == Some(resource))
    );
    assert!(records.iter().all(|record| record.state == "valid"));
    // Re-enter actual admission with retained valid records: this exercises the UPDATE
    // branch rather than incorrectly treating an INSERT result as UPDATE evidence.
    {
        let connection = target.connection.write();
        connection
            .execute(
                /* sqlite-large-value: allow-for-now lv-hba0e63fdf14f owner=st3::store::resource_pending_receive_tests TODO=bound-or-reference-values-and-reclamation */ "INSERT INTO local_resource_projection_pending(subject) VALUES(?1)",
                [resource],
            )
            .unwrap();
        connection
            .execute(/* sqlite-large-value: allow-for-now lv-h07596fa706d2 owner=st3::store::resource_pending_receive_tests TODO=chunk-or-reference-exact-wire-values */ "UPDATE replica_envelopes SET receipt_state='pending'", [])
            .unwrap();
    }
    drop(target);
    let target = Store::open(&path, "peer").unwrap();
    check_triggers(&target);
    target
        .connection
        .write()
        .execute(
            /* sqlite-large-value: allow-for-now lv-h81c363d12b81 owner=st3::store::resource_pending_receive_tests TODO=bound-or-reference-values-and-reclamation */ "INSERT INTO local_resource_projection_pending(subject) VALUES(?1)",
            [resource],
        )
        .unwrap();
    target.validate_replication_backlog().unwrap();
    assert!(target.project_replication_backlog().unwrap());
    assert!(target.replica_records(true).unwrap().is_empty());
    let after = target.replica_records(false).unwrap();
    assert_eq!(
        serde_json::to_value(&after).unwrap(),
        serde_json::to_value(&records).unwrap()
    );
    assert_eq!(
        source.replication_inventory().unwrap().digest,
        target.replication_inventory().unwrap().digest,
        "same sealed envelope cut"
    );
    target.replay_replication_graph().unwrap();
    assert_eq!(
        target.latest_actual_value(resource).unwrap().unwrap()["facts"]["state"],
        "open"
    );
    assert!(target.replica_records(true).unwrap().is_empty());
    let reader = target.readers.get();
    assert_eq!(
        projection_digest::tables(&reader).unwrap(),
        projection_digest::oracle(&reader).unwrap()
    );
}
