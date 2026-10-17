//! Actual independent update creation, replication and retained-history replay controls.
use super::*;

fn post_update(store: &Store) -> ClaimRecord {
    post_update_because(store, "The requesting work ended.")
}

fn post_update_because(store: &Store, reason: &str) -> ClaimRecord {
    let update = store
        .connection
        .batched(|tx| {
            store.append_update_tx(
                tx,
                "daemon/runtime",
                "person/avery",
                "Ask ended",
                reason,
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

fn definition_row(store: &Store, spec: &MissionSpec) -> (String, String, String) {
    store
        .readers
        .get()
        .query_row(
            "SELECT revision,state,claim_id FROM mission_definitions WHERE mission_id=?1",
            [&spec.id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap()
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
        assert_eq!(definition_row(&target, &spec).2, canonical.id);
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

#[test]
fn person_definition_source_repairs_even_when_revision_history_is_already_canonical() {
    let (first, second, a, b) = independent_updates();
    let target = receive_both(&first, &second);
    let spec = mission(&a);
    let (canonical, later) = ordered_claims(&target, &a, &b);
    let history = revision_row(&target, &spec);
    let expected = definition_row(&target, &spec);
    // Model a retained definition from an incremental rebuild while history already agrees.
    target
        .connection
        .batched(|tx| -> Result<()> {
            assert_eq!(
                tx.execute(
                    "UPDATE mission_definitions SET claim_id=?2 WHERE mission_id=?1",
                    params![spec.id, later.id],
                )?,
                1
            );
            Ok(())
        })
        .unwrap()
        .unwrap();
    let mut before_tables = projection_digest::tables(&target.readers.get()).unwrap();
    let fields = &canonical.body["fields"];
    target
        .connection
        .batched(|tx| {
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
        })
        .unwrap()
        .unwrap();
    assert_eq!(definition_row(&target, &spec), expected);
    assert_eq!(revision_row(&target, &spec), history);
    let mut after_tables = projection_digest::tables(&target.readers.get()).unwrap();
    assert_ne!(
        before_tables.remove("mission_definitions"),
        after_tables.remove("mission_definitions")
    );
    assert_eq!(before_tables, after_tables);
    target.replay_replication_graph().unwrap();
    assert_eq!(definition_row(&target, &spec), expected);
    assert_eq!(revision_row(&target, &spec), history);
}

#[test]
fn independent_person_updates_of_two_revisions_select_the_canonical_definition_however_they_arrive()
{
    // Each daemon words an ended ask's reason from its own view, so one update key can carry two
    // revisions of the same internal mission.
    let alder = Store::open_memory("alder").unwrap();
    let birch = Store::open_memory("birch").unwrap();
    let a = post_update_because(&alder, "The requester was stopped or retired.");
    let b = post_update_because(&birch, "The run it belonged to ended.");
    assert_eq!(a.subject, b.subject);
    let (spec_a, spec_b) = (mission(&a), mission(&b));
    assert_eq!(spec_a.id, spec_b.id);
    assert_ne!(spec_a.revision, spec_b.revision);

    // One claim per exchange, in both arrival orders. `receive` also asserts after each exchange
    // that every projected table equals a full replay's.
    let mut members = Vec::new();
    for (first, second) in [(&alder, &birch), (&birch, &alder)] {
        let target = Store::open_memory("cedar").unwrap();
        target.project_replication_backlog().unwrap();
        super::tests::receive(first, &target);
        super::tests::receive(second, &target);
        members.push(target);
    }
    // Both claims in one exchange, from a member that already holds them.
    let dove = Store::open_memory("dove").unwrap();
    dove.project_replication_backlog().unwrap();
    super::tests::receive(&birch, &dove);
    super::tests::receive(&alder, &dove);
    let elm = Store::open_memory("elm").unwrap();
    elm.project_replication_backlog().unwrap();
    super::tests::receive(&dove, &elm);
    members.push(elm);
    members.push(dove);
    // Each writer projected its own claim when it wrote it, then receives the other's. A writer
    // has no healthy graph frontier until it first projects, so that initial replay is
    // intentional and happens before it receives, as for every other target.
    alder.project_replication_backlog().unwrap();
    birch.project_replication_backlog().unwrap();
    super::tests::receive(&birch, &alder);
    super::tests::receive(&alder, &birch);

    let (canonical, _) = ordered_claims(&alder, &a, &b);
    let expected = (
        mission(&canonical).revision,
        "ready".to_owned(),
        canonical.id.clone(),
    );
    let digest = projection_digest::tables(&alder.readers.get()).unwrap();
    for member in members.iter().chain([&birch]) {
        assert_eq!(definition_row(member, &spec_a), expected);
        // Both revisions stay in history, each from its own claim.
        assert_eq!(revision_row(member, &spec_a).2, a.id);
        assert_eq!(revision_row(member, &spec_b).2, b.id);
        assert_eq!(
            projection_digest::tables(&member.readers.get()).unwrap(),
            digest
        );
    }
    assert_eq!(definition_row(&alder, &spec_a), expected);
    alder.replay_replication_graph().unwrap();
    assert_eq!(
        projection_digest::tables(&alder.readers.get()).unwrap(),
        digest
    );
}

/// Publish `mission_id` on `store` with a revision below `below`: the order in which comparing
/// (revision, claim id) alone would keep a runtime definition already projected.
fn publish_below(store: &Store, mission_id: &str, below: &str) -> MissionSpec {
    let (source, intent) = (0..)
        .map(|attempt| {
            let source = format!(
                "version 2\nmission {mission_id:?} state=\"ready\" {{\n  goal \"Published under a \
                 runtime update's id ({attempt}).\";\n  step \"update\" {{ agentless }}\n}}\n"
            );
            let intent = crate::graph::parse_intent(&source, "birch").unwrap();
            (source, intent)
        })
        .find(|(_, intent)| intent.missions[mission_id].revision.as_str() < below)
        .unwrap();
    let planned = store
        .mission(
            &intent,
            crate::model::IntentInput {
                kdl: source,
                source_name: None,
            },
        )
        .unwrap();
    store
        .apply(&intent, &planned.subject_tokens, "same-id-publication")
        .unwrap();
    intent.missions[mission_id].clone()
}

#[test]
fn a_published_definition_of_a_runtime_missions_id_stays_whichever_claim_arrives_first() {
    // A synthetic guard fixture: runtime missions use hash-named ids such as person-update/…,
    // which nothing reserves, so only a deliberately named publication can share one. Its
    // definition must stay even when a runtime update of that id is canonically earlier.
    let alder = Store::open_memory("alder").unwrap();
    let birch = Store::open_memory("birch").unwrap();
    let runtime = post_update(&alder);
    let runtime_spec = mission(&runtime);
    let published = publish_below(&birch, &runtime_spec.id, &runtime_spec.revision);
    assert_ne!(published.revision, runtime_spec.revision);
    let publication = definition_row(&birch, &published).2;

    let mut members = Vec::new();
    for (first, second) in [(&alder, &birch), (&birch, &alder)] {
        let target = Store::open_memory("cedar").unwrap();
        target.project_replication_backlog().unwrap();
        // `receive` also asserts after each exchange that every projected table equals a full
        // replay's.
        super::tests::receive(first, &target);
        super::tests::receive(second, &target);
        members.push(target);
    }
    // A writer has no healthy graph frontier until it first projects, so that initial replay is
    // intentional and happens before it receives, as for every other target.
    alder.project_replication_backlog().unwrap();
    birch.project_replication_backlog().unwrap();
    super::tests::receive(&birch, &alder);
    super::tests::receive(&alder, &birch);
    {
        let reader = alder.readers.get();
        assert!(
            canonical::claim_key(&reader, &runtime.id).unwrap()
                < canonical::claim_key(&reader, &publication).unwrap(),
            "the runtime update is canonically first"
        );
    }

    let expected = (published.revision.clone(), "ready".to_owned(), publication);
    let digest = projection_digest::tables(&alder.readers.get()).unwrap();
    for member in members.iter().chain([&alder, &birch]) {
        assert_eq!(definition_row(member, &published), expected);
        // The definition's revision still finds its body, and the runtime revision keeps its
        // history row from its own claim.
        let body: String = member
            .readers
            .get()
            .query_row(
                "SELECT r.body FROM mission_definitions d JOIN mission_revisions r
                 ON r.mission_id=d.mission_id AND r.revision=d.revision WHERE d.mission_id=?1",
                [&published.id],
                |row| row.get(0),
            )
            .unwrap();
        let body: MissionSpec = serde_json::from_str(&body).unwrap();
        assert_eq!(body.revision, published.revision);
        assert_eq!(revision_row(member, &runtime_spec).2, runtime.id);
        assert_eq!(
            projection_digest::tables(&member.readers.get()).unwrap(),
            digest
        );
    }
    alder.replay_replication_graph().unwrap();
    assert_eq!(definition_row(&alder, &published), expected);
    assert_eq!(
        projection_digest::tables(&alder.readers.get()).unwrap(),
        digest
    );
}
