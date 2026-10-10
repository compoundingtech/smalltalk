//! Actual independent update creation, replication and retained-history replay controls.
use super::*;

fn post_update(store: &Store) -> ClaimRecord {
    let update = store
        .connection
        .batched(|tx| {
            store.append_update_tx(
                tx,
                "daemon/runtime",
                "person/avery",
                "Ask ended",
                "The requesting work ended.",
                "same-ended-ask",
                &json!({"version": 1, "type": "update", "about": "step-run/fixture/ask"}),
            )
        })
        .unwrap()
        .unwrap();
    request(&store.readers.get(), &update.subject)
        .unwrap()
        .unwrap()
}

fn independent_updates() -> (Store, Store, ClaimRecord, ClaimRecord) {
    let first = Store::open_memory("alder").unwrap();
    let second = Store::open_memory("birch").unwrap();
    let a = post_update(&first);
    let b = post_update(&second);
    assert_ne!(a.id, b.id);
    assert_eq!(a.subject, b.subject);
    assert_eq!(a.body, b.body);
    (first, second, a, b)
}

fn mission(claim: &ClaimRecord) -> MissionSpec {
    serde_json::from_value(claim.body["fields"]["mission_spec"].clone()).unwrap()
}

// The local creation index is intentionally included in this history-preservation oracle.
fn revision_row(store: &Store, mission: &MissionSpec) -> (String, String, String, u64) {
    store
        .readers
        .get()
        .query_row(
            "SELECT state,body,claim_id,created_index FROM mission_revisions
             WHERE mission_id=?1 AND revision=?2",
            params![mission.id, mission.revision],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap()
}

fn ordered_claims(store: &Store, a: &ClaimRecord, b: &ClaimRecord) -> (ClaimRecord, ClaimRecord) {
    let reader = store.readers.get();
    if canonical::claim_key(&reader, &a.id).unwrap() < canonical::claim_key(&reader, &b.id).unwrap()
    {
        (a.clone(), b.clone())
    } else {
        (b.clone(), a.clone())
    }
}

fn receive_both(first: &Store, second: &Store) -> Store {
    let target = Store::open_memory("cedar").unwrap();
    target.project_replication_backlog().unwrap();
    super::tests::receive(first, &target);
    super::tests::receive(second, &target);
    target
}

#[test]
fn independent_person_updates_select_one_revision_source_in_both_arrival_orders() {
    let (first, second, a, b) = independent_updates();
    let spec = mission(&a);
    let mut digests = Vec::new();
    for (source, other) in [(&first, &second), (&second, &first)] {
        let target = Store::open_memory("cedar").unwrap();
        target.project_replication_backlog().unwrap();
        // This helper also compares every projected table with actual full replay.
        super::tests::receive(source, &target);
        let before = revision_row(&target, &spec);
        super::tests::receive(other, &target);
        let (canonical, _) = ordered_claims(&target, &a, &b);
        let after = revision_row(&target, &spec);
        assert_eq!(after.2, canonical.id);
        assert_eq!(
            (&after.0, &after.1, after.3),
            (&before.0, &before.1, before.3)
        );
        assert_eq!(after.1, serde_json::to_string(&spec).unwrap());
        digests.push(
            projection_digest::tables(&target.readers.get()).unwrap()["mission_revisions"].clone(),
        );
        // Repeating a full replay neither changes the source nor appends revision history.
        target.replay_replication_graph().unwrap();
        assert_eq!(revision_row(&target, &spec), after);
    }
    assert_eq!(digests[0], digests[1]);
}

fn retain_noncanonical_source(store: &Store, spec: &MissionSpec, claim: &ClaimRecord) {
    // Model the existing first-arrival projection; the two actual source claims stay intact.
    store
        .connection
        .batched(|tx| -> Result<()> {
            assert_eq!(
                tx.execute(
                    "UPDATE mission_revisions SET claim_id=?3 WHERE mission_id=?1 AND revision=?2",
                    params![spec.id, spec.revision, claim.id],
                )?,
                1
            );
            Ok(())
        })
        .unwrap()
        .unwrap();
}

#[test]
fn full_replay_repairs_retained_person_revision_source_without_replacing_history() {
    let (first, second, a, b) = independent_updates();
    let target = receive_both(&first, &second);
    let spec = mission(&a);
    let (canonical, later) = ordered_claims(&target, &a, &b);
    let expected = revision_row(&target, &spec);
    retain_noncanonical_source(&target, &spec, &later);
    assert_eq!(revision_row(&target, &spec).2, later.id);
    target.replay_replication_graph().unwrap();
    assert_eq!(revision_row(&target, &spec).2, canonical.id);
    assert_eq!(revision_row(&target, &spec), expected);
}

#[test]
fn person_revision_source_repair_rolls_back_and_preserves_other_projection_tables() {
    let (first, second, a, b) = independent_updates();
    let target = receive_both(&first, &second);
    let spec = mission(&a);
    let (canonical, later) = ordered_claims(&target, &a, &b);
    retain_noncanonical_source(&target, &spec, &later);
    let before = revision_row(&target, &spec);
    let mut before_tables = projection_digest::tables(&target.readers.get()).unwrap();
    let fields = &canonical.body["fields"];
    let project = |tx: &Transaction<'_>| {
        project_minimal_run(
            tx,
            &canonical,
            &spec,
            fields["run"]
                .as_str()
                .unwrap()
                .trim_start_matches("mission-run/"),
            fields["generation"]
                .as_str()
                .unwrap()
                .trim_start_matches("run-generation/"),
            fields["run"]
                .as_str()
                .unwrap()
                .trim_start_matches("mission-run/"),
            "person ask",
        )
    };
    let refused = target
        .connection
        .batched(|tx| -> Result<()> {
            project(tx).map_err(|error| anyhow::anyhow!(error.to_string()))?;
            let selected: String = tx.query_row(
                "SELECT claim_id FROM mission_revisions WHERE mission_id=?1 AND revision=?2",
                params![spec.id, spec.revision],
                |row| row.get(0),
            )?;
            assert_eq!(selected, canonical.id);
            anyhow::bail!("fixture refuses the transaction after source selection")
        })
        .unwrap();
    assert!(refused.is_err());
    assert_eq!(revision_row(&target, &spec), before);
    assert_eq!(
        projection_digest::tables(&target.readers.get()).unwrap(),
        before_tables
    );

    target.connection.batched(project).unwrap().unwrap();
    let after = revision_row(&target, &spec);
    assert_eq!(after.2, canonical.id);
    assert_eq!(
        (&after.0, &after.1, after.3),
        (&before.0, &before.1, before.3)
    );
    let mut after_tables = projection_digest::tables(&target.readers.get()).unwrap();
    assert_ne!(
        before_tables.remove("mission_revisions"),
        after_tables.remove("mission_revisions")
    );
    assert_eq!(before_tables, after_tables);
}
