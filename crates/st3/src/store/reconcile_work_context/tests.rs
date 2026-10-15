use super::*;
use smallclaims::sqlite::work::{SqliteWork, SqliteWorkScope};
use std::time::Instant;

const ACTOR: &str = "agent/alder.worker";
const SEED: u64 = 0x2051_2481_2511;

fn fixture(count: usize) -> Store {
    let store = Store::open_memory("alder").unwrap();
    if count > 0 {
        add_run(&store, count, "batch-work");
    }
    store
}

fn add_run(store: &Store, count: usize, name: &str) {
    let steps = (0..count)
        .map(|n| {
            format!("step \"task-{n:05}\" {{ assigned-to \"{ACTOR}\"; goal \"Check item {n}.\"; }}")
        })
        .collect::<Vec<_>>()
        .join("\n");
    let text = format!(
        "version 2\nmission \"{name}\" state=\"ready\" {{ goal \"Process work.\"; {steps} }}"
    );
    let intent = crate::graph::parse_internal_intent(&text, "alder").unwrap();
    store
        .apply_internal(&intent, &format!("{name}-definition"))
        .unwrap();
    store
        .create_mission_run(&MissionRunRequest {
            mission: name.into(),
            revision: None,
            workspace: "/tmp".into(),
            requester: Some(ACTOR.into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: format!("{name}-run"),
        })
        .unwrap();
}

fn oracle(store: &Store, actor: &str, at: u128) -> Result<Vec<StepRunView>> {
    // Exact existing per-step path, including owner/definition queries and all enrichments.
    store.read_snapshot(|_| store.work_at_snapshot_internal(Some(actor), true, at, false))
}

fn parity(store: &Store, actor: &str, at: u128) -> Vec<StepRunView> {
    let (old, old_keys) = smallclaims::touched::record(|| oracle(store, actor, at).unwrap());
    let (new, new_keys) = smallclaims::touched::record(|| read(store, actor, at).unwrap());
    assert!(
        old_keys.is_subset(&new_keys),
        "batched read dropped registered dependencies: {:?}",
        old_keys.difference(&new_keys).collect::<Vec<_>>()
    );
    println!(
        "registered work context keys: old={} new={} added={:?}",
        old_keys.len(),
        new_keys.len(),
        new_keys.difference(&old_keys).collect::<Vec<_>>()
    );
    assert_eq!(
        serde_json::to_value(&new).unwrap(),
        serde_json::to_value(&old).unwrap()
    );
    new
}

fn measured<T>(read: impl FnOnce() -> T) -> (T, SqliteWork, std::time::Duration) {
    let started = Instant::now();
    let scope = SqliteWorkScope::start();
    let value = read();
    (value, scope.finish(), started.elapsed())
}

#[test]
fn batched_work_metadata_matches_every_boundary_and_seeded_effective_state() {
    let at = now_ms();
    for count in [0, 1, 127, 128, 129, 256] {
        let store = fixture(count);
        let baseline = parity(&store, ACTOR, at);
        assert_eq!(baseline.len(), count);
        let mut seed = SEED;
        {
            let connection = store.connection.write();
            for view in &baseline {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                let status = [
                    "pending",
                    "ready",
                    "claimed",
                    "working",
                    "verifying",
                    "blocked",
                    "waiting-person",
                    "completed",
                    "failed",
                    "cancelled",
                ][seed as usize % 10];
                let expiry = if seed & 1 == 0 {
                    at.saturating_sub(1)
                } else {
                    at + 60_000
                };
                connection
                    .execute(
                        /* sqlite-large-value: allow-for-now lv-h830d430159b2 owner=st3::store::reconcile_work_context::tests TODO=bound-or-reference-values-and-reclamation */ "UPDATE step_runs SET status=?2,lease_owner=?3,
                    lease_incarnation='worker-one',lease_expires_at_unix_ms=?4,
                    blocked_reason=?5 WHERE subject=?1",
                        params![
                            view.subject,
                            status,
                            ACTOR,
                            expiry.to_string(),
                            (seed & 2 == 0).then_some("check blockers")
                        ],
                    )
                    .unwrap();
            }
        }
        parity(&store, ACTOR, at);
        parity(&store, ACTOR, at + 60_001);
        for (column, value) in [
            ("phase", "revision-draining"),
            ("phase", "terminal"),
            ("status", "completed"),
        ] {
            store
                .connection
                .write()
                .execute(&format!(/* sqlite-large-value: allow-for-now lv-h690bc554ac2b owner=st3::store::reconcile_work_context::tests TODO=bound-or-reference-values-and-reclamation */ "UPDATE mission_runs SET {column}=?1"), [value])
                .unwrap();
            parity(&store, ACTOR, at);
        }
    }
    println!("seeded full-vector parity seed={SEED:#x}");
}

#[test]
fn batched_work_metadata_keeps_missing_rows_duplicates_and_errors() {
    let store = fixture(2);
    add_run(&store, 1, "other-work");
    let at = now_ms();
    let original = parity(&store, ACTOR, at);
    let mut requests = vec![original[0].clone(); 129];
    requests[128] = original.last().unwrap().clone();
    requests[126].run = "mission-run/missing".into();
    requests[127].generation = "run-generation/missing".into();
    let connection = store.readers.get();
    let actual = owners(&connection, &requests).unwrap();
    for (view, owner) in requests.iter().zip(actual) {
        let mut old = view.clone();
        let mut new = view.clone();
        apply_effective_step_state(&connection, &mut old, at).unwrap();
        apply_effective_step_state_with_owner(&connection, &mut new, at, owner).unwrap();
        assert_eq!(
            serde_json::to_value(old).unwrap(),
            serde_json::to_value(new).unwrap()
        );
    }
    for count in [0, 1, 127, 128, 129, 256] {
        let boundary = (0..count)
            .map(|n| {
                let mut view = original[0].clone();
                view.run = format!("mission-run/missing-{n}");
                view
            })
            .collect::<Vec<_>>();
        let selected = owners(&connection, &boundary).unwrap();
        assert_eq!(selected.len(), count);
        assert!(selected.into_iter().all(|owner| owner.is_none()));
    }
    drop(connection);
    let old_body: String = store
        .readers
        .get()
        .query_row("SELECT body FROM mission_revisions", [], |r| r.get(0))
        .unwrap();
    store
        .connection
        .write()
        .execute(/* sqlite-large-value: allow-for-now lv-h7559a9f58991 owner=st3::store::reconcile_work_context::tests TODO=bound-or-reference-values-and-reclamation */ "UPDATE mission_revisions SET body='not a mission'", [])
        .unwrap();
    for view in &original {
        let error = enrich_step_definition(&store.readers.get(), &mut view.clone()).unwrap_err();
        let result = read(&store, ACTOR, at);
        assert!(
            result.is_err(),
            "a malformed pinned definition cannot return a partial vector"
        );
        assert_eq!(result.unwrap_err().to_string(), error.to_string());
    }
    store
        .connection
        .write()
        .execute(/* sqlite-large-value: allow-for-now lv-h42174b4fa512 owner=st3::store::reconcile_work_context::tests TODO=bound-or-reference-values-and-reclamation */ "UPDATE mission_revisions SET body=?1", [old_body])
        .unwrap();
    parity(&store, ACTOR, at);
    store
        .connection
        .write()
        .execute(/* sqlite-large-value: allow-for-now lv-he542e34282ca owner=st3::store::reconcile_work_context::tests TODO=bound-or-reference-values-and-reclamation */ "DELETE FROM mission_revisions", [])
        .unwrap();
    parity(&store, ACTOR, at);
}

#[test]
fn batched_work_metadata_reads_new_owner_and_definition_on_each_call() {
    let store = fixture(2);
    let at = now_ms();
    let before = parity(&store, ACTOR, at);
    let connection = store.connection.write();
    let body: String = connection
        .query_row("SELECT body FROM mission_revisions", [], |r| r.get(0))
        .unwrap();
    let mut mission: MissionSpec = serde_json::from_str(&body).unwrap();
    for step in mission.steps.values_mut() {
        step.fresh_context = true;
        step.timeout_ms = Some(12345);
    }
    connection
        .execute(
            /* sqlite-large-value: allow-for-now lv-h36c5eb76ca4d owner=st3::store::reconcile_work_context::tests TODO=bound-or-reference-values-and-reclamation */ "UPDATE mission_revisions SET body=?1",
            [serde_json::to_string(&mission).unwrap()],
        )
        .unwrap();
    drop(connection);
    let changed = parity(&store, ACTOR, at);
    assert_ne!(
        serde_json::to_value(before).unwrap(),
        serde_json::to_value(&changed).unwrap()
    );
    assert!(
        changed
            .iter()
            .all(|view| view.fresh_context && view.timeout_ms == Some(12345))
    );
    store
        .connection
        .write()
        .execute(/* sqlite-large-value: allow-for-now lv-he865c6cc15fd owner=st3::store::reconcile_work_context::tests TODO=bound-or-reference-values-and-reclamation */ "UPDATE run_generations SET status='completed'", [])
        .unwrap();
    assert!(
        parity(&store, ACTOR, at)
            .iter()
            .all(|view| view.status == "cancelled")
    );
}

#[test]
fn batched_work_metadata_keeps_real_person_blockers_and_responses() {
    for cancelled in [false, true] {
        let (store, _origin, input) = person_work::tests::fixture();
        let before = parity(&store, &input.actor, now_ms());
        let ask = store.ask_person(&input).unwrap();
        let blocked = parity(&store, &input.actor, now_ms());
        assert_ne!(
            serde_json::to_value(before).unwrap(),
            serde_json::to_value(blocked).unwrap()
        );
        store
            .finish_person_step(
                &crate::model::PersonStepResponse {
                    delegation: None,
                    subject: ask.subject,
                    actor: if cancelled {
                        input.actor.clone()
                    } else {
                        "person/avery".into()
                    },
                    summary: "Friday".into(),
                    evidence: Vec::new(),
                    episode: None,
                    answer: None,
                    idempotency_key: "batch-person-done".into(),
                },
                cancelled,
            )
            .unwrap();
        let answered = parity(&store, &input.actor, now_ms());
        assert!(answered.iter().any(|view| !view.person_answers.is_empty()));
        // The ask itself reads its own response, whereas the originating step reads by origin.
        let own = store
            .step_run(
                &answered
                    .iter()
                    .flat_map(|v| &v.person_answers)
                    .next()
                    .unwrap()
                    .ask,
            )
            .unwrap()
            .unwrap();
        let connection = store.readers.get();
        assert!(support(&connection, std::slice::from_ref(&own)).unwrap()[0].1);
        let mut own_old = own.clone();
        let mut own_new = own.clone();
        person_work::enrich_responses(&connection, &mut own_old).unwrap();
        if support(&connection, std::slice::from_ref(&own_new)).unwrap()[0].1 {
            person_work::enrich_responses(&connection, &mut own_new).unwrap();
        }
        assert_eq!(
            serde_json::to_value(own_old).unwrap(),
            serde_json::to_value(own_new).unwrap()
        );
        drop(connection);
        let at = now_ms();
        store.connection.write().execute(
            /* sqlite-large-value: allow-for-now lv-h2c33e0babb4a owner=st3::store::reconcile_work_context::tests TODO=prepare-body-and-predecessor-values */ "UPDATE claims SET accepted_at_unix_ms=?1 WHERE kind IN ('work.person-done','work.person-cancelled')",
            [(at + 10_000).to_string()],
        ).unwrap();
        // Unlike handoff, the existing response helper has no clock filter: an admitted
        // response after `at` still enriches. The broad request proof must retain it.
        assert!(
            parity(&store, &input.actor, at)
                .iter()
                .any(|v| !v.person_answers.is_empty())
        );
        let request = store
            .latest_claim(&own.subject, Some("work.person-asked"))
            .unwrap()
            .unwrap();

        store.connection.write().execute(
            /* sqlite-large-value: allow-for-now lv-h6ae90f48ce93 owner=st3::store::reconcile_work_context::tests TODO=prepare-body-and-predecessor-values */ "UPDATE claims SET body=json_set(body,'$.fields.origin_step','step-run/another-origin') WHERE id=?1",
            [&request.id],
        ).unwrap();
        let moved_origin = parity(&store, &input.actor, at);
        assert!(moved_origin.iter().all(|v| v.person_answers.is_empty()));
        assert!(
            support(&store.readers.get(), &moved_origin)
                .unwrap()
                .iter()
                .all(|row| !row.1)
        );
        store
            .connection
            .write()
            .execute(
                /* sqlite-large-value: allow-for-now lv-h7093e34af13c owner=st3::store::reconcile_work_context::tests TODO=prepare-body-and-predecessor-values */ "UPDATE claims SET body=?2 WHERE id=?1",
                params![request.id, serde_json::to_string(&request.body).unwrap()],
            )
            .unwrap();
        store
            .connection
            .write()
            .execute(
                /* sqlite-large-value: allow-for-now lv-h32c2438313b9 owner=st3::store::reconcile_work_context::tests TODO=prepare-body-and-predecessor-values */ "UPDATE claims SET body=json_set(body,'$.fields.origin_attempt',?2) WHERE id=?1",
                params![request.id, 99],
            )
            .unwrap();
        let stale = parity(&store, &input.actor, at);
        assert!(stale.iter().all(|v| v.person_answers.is_empty()));
        assert!(
            support(&store.readers.get(), &stale)
                .unwrap()
                .iter()
                .any(|row| row.1)
        );
        // Reader-only corruption/retraction fixture: an answer without its joined request
        // cannot enrich either path. This does not claim production retraction/replay coverage.
        store
            .connection
            .write()
            .execute(/* sqlite-large-value: allow-for-now lv-h98356783d74d owner=st3::store::reconcile_work_context::tests TODO=prepare-body-and-predecessor-values */ "DELETE FROM claims WHERE kind='work.person-asked'", [])
            .unwrap();
        let orphaned = parity(&store, &input.actor, now_ms());
        assert!(orphaned.iter().all(|view| view.person_answers.is_empty()));
        assert!(
            support(&store.readers.get(), &orphaned)
                .unwrap()
                .iter()
                .all(|row| !row.1)
        );
    }
}

#[test]
fn batched_work_metadata_reduces_whole_read_statements_on_128_1000_4200_steps() {
    for count in [128_usize, 1000, 4200] {
        let store = fixture(count);
        let at = now_ms();
        MAX_SNAPSHOT_CALL_NS.with(|maximum| maximum.set(0));
        let (first_old, first_old_work, first_old_time) =
            measured(|| oracle(&store, ACTOR, at).unwrap());
        let (first_new, first_new_work, first_new_time) =
            measured(|| read(&store, ACTOR, at).unwrap());
        assert_eq!(
            serde_json::to_value(first_new).unwrap(),
            serde_json::to_value(first_old).unwrap()
        );
        parity(&store, ACTOR, at);
        let (old, old_work, old_time) = measured(|| oracle(&store, ACTOR, at).unwrap());
        let (new, new_work, new_time) = measured(|| read(&store, ACTOR, at).unwrap());
        assert_eq!(
            serde_json::to_value(new).unwrap(),
            serde_json::to_value(old).unwrap()
        );
        // This fixture has one exact run/generation pair and one pinned definition.
        let batched_statements = 2 + count.div_ceil(CHUNK) as u64;
        println!(
            "{count}-step first calls (after fixture setup, not cold database): old {first_old_work:?} {first_old_time:?}; new {first_new_work:?} {first_new_time:?}; warm old: {old_work:?} {old_time:?}; warm new: {new_work:?} {new_time:?}"
        );
        MAX_SNAPSHOT_CALL_NS.with(|maximum| {
            println!(
                "{count}-step maximum candidate snapshot-call upper bound: {} ns",
                maximum.get()
            )
        });
        assert!(new_work.statements > 0);
        assert_eq!(
            old_work.statements,
            new_work.statements + 4 * count as u64 - batched_statements
        );
        assert!(
            new_work.vm_steps < old_work.vm_steps,
            "old={old_work:?} new={new_work:?}"
        );
    }
}

#[test]
fn batched_work_metadata_plan_seeks_run_generation_root_and_definition() {
    let store = fixture(129);
    let connection = store.readers.get();
    for query in [OWNERS, DEFINITIONS, PHASES, SUPPORT] {
        let plan = connection
            .prepare(/* sqlite-large-value: allow-for-now lv-h16ff0b53e4d9 owner=st3::store::reconcile_work_context::tests TODO=trace-statement-and-value-representation */ &format!("EXPLAIN QUERY PLAN {query}"))
            .unwrap()
            .query_map(["[]"], |row| row.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        println!("{query}\n{plan:#?}");
        assert!(
            plan.iter()
                .any(|line| line.starts_with("SCAN requested VIRTUAL TABLE")),
            "{plan:?}"
        );
        for alias in if query == OWNERS {
            vec!["runs", "generations", "roots"]
        } else if query == DEFINITIONS {
            vec!["generations", "runs", "revisions"]
        } else if query == PHASES {
            vec!["mission_runs"]
        } else {
            vec!["released", "asked", "origin"]
        } {
            assert!(
                plan.iter()
                    .any(|line| line.starts_with(&format!("SEARCH {alias} "))),
                "{plan:?}"
            );
        }
        assert!(
            plan.iter().all(|line| !line.starts_with("SCAN ")
                || line.starts_with("SCAN requested VIRTUAL TABLE")),
            "{plan:?}"
        );
        assert!(
            plan.iter().all(|line| !line.contains("TEMP B-TREE")),
            "{plan:?}"
        );
    }
}

#[test]
fn batched_work_metadata_keeps_seeded_real_handoff_carry_and_supersession() {
    use crate::model::{WorkAcknowledgeRequest, WorkHandoffRequest};
    const PEER: &str = "agent/alder.peer";
    let store = fixture(16);
    let agents = crate::graph::parse_internal_intent(
        "version 2\nagent \"alder.worker\" { workspace \"/tmp\"; command \"true\" }\nagent \"alder.peer\" { workspace \"/tmp\"; command \"true\" }", "alder").unwrap();
    store.apply_internal(&agents, "batch-actors").unwrap();
    let initial = parity(&store, ACTOR, now_ms());
    for view in &initial {
        store.set_step_state(&view.subject, "ready", None).unwrap();
    }
    let pick = SEED as usize % initial.len();
    let carried = &initial[pick];
    let handed = &initial[(pick + 1) % initial.len()];
    let claim = |actor: &str, key: &str| WorkRequest {
        wait_ms: None,
        actor: Some(actor.into()),
        incarnation: Some(format!("{actor}-one")),
        summary: None,
        reason: None,
        evidence: Vec::new(),
        idempotency_key: key.into(),
    };
    store
        .work_action(&handed.subject, "claim", &claim(ACTOR, "handoff-claim"))
        .unwrap();
    for (from, to, key) in [(ACTOR, PEER, "to-peer"), (PEER, ACTOR, "to-worker")] {
        store
            .handoff_work(
                &handed.subject,
                &WorkHandoffRequest {
                    actor: from.into(),
                    incarnation: Some(format!("{from}-one")),
                    to: to.into(),
                    note: "Review the same work next.".into(),
                    evidence: Vec::new(),
                    idempotency_key: key.into(),
                },
            )
            .unwrap();
        let source = store
            .latest_claim(&handed.subject, Some("work.released"))
            .unwrap()
            .unwrap();
        parity(&store, ACTOR, now_ms());
        parity(&store, PEER, now_ms());
        store
            .acknowledge_work(
                &handed.subject,
                &WorkAcknowledgeRequest {
                    actor: to.into(),
                    message: source.body["fields"]["handoff_message"]
                        .as_str()
                        .unwrap()
                        .into(),
                },
            )
            .unwrap();
        if to == PEER {
            store
                .work_action(&handed.subject, "claim", &claim(PEER, "peer-claim"))
                .unwrap();
        }
    }
    let handoff_view = parity(&store, ACTOR, now_ms());
    assert!(
        handoff_view
            .iter()
            .find(|view| view.subject == handed.subject)
            .unwrap()
            .constraints
            .iter()
            .any(|line| line.contains("Handoff from"))
    );
    // The ordinary admission rule permits only one independent claim per agent.
    // Both transfers have released the handed step before this separate carry claim.
    store
        .work_action(&carried.subject, "claim", &claim(ACTOR, "carry-claim"))
        .unwrap();
    // Explicit draining control: a claimed step remains while its adjacent ready step leaves.
    let original_phase: String = store
        .readers
        .get()
        .query_row("SELECT phase FROM mission_runs", [], |row| row.get(0))
        .unwrap();
    store
        .connection
        .write()
        .execute(/* sqlite-large-value: allow-for-now lv-h35774d4576b2 owner=st3::store::reconcile_work_context::tests TODO=bound-or-reference-values-and-reclamation */ "UPDATE mission_runs SET phase='revision-draining'", [])
        .unwrap();
    let draining = parity(&store, ACTOR, now_ms());
    assert!(draining.iter().any(|view| view.subject == carried.subject));
    assert!(draining.iter().all(|view| view.status != "ready"));
    store
        .connection
        .write()
        .execute(/* sqlite-large-value: allow-for-now lv-he52d7628af81 owner=st3::store::reconcile_work_context::tests TODO=bound-or-reference-values-and-reclamation */ "UPDATE mission_runs SET phase=?1", [&original_phase])
        .unwrap();
    // Admission of an unreported verifying state retains the original claim/lease;
    // ordinary revision adoption emits its carried-claimant evidence.
    store
        .set_step_state(&carried.subject, "verifying", None)
        .unwrap();
    let steps = (0..16)
        .map(|n| {
            format!("step \"task-{n:05}\" {{ assigned-to \"{ACTOR}\"; goal \"Check item {n}.\"; }}")
        })
        .collect::<Vec<_>>()
        .join("\n");
    let changed = crate::graph::parse_internal_intent(&format!("version 2\nmission \"batch-work\" state=\"ready\" {{ goal \"Process work.\"; constraint \"Retain completed work.\"; {steps} }}"), "alder").unwrap();
    store
        .apply_internal(&changed, "batch-new-revision")
        .unwrap();
    let revised = store
        .adopt_mission_revision(
            &carried.run,
            &changed.missions["batch-work"],
            ACTOR,
            "Update guidance",
            "batch-adoption",
        )
        .unwrap();
    assert_ne!(revised.generation, carried.generation);
    let views = parity(&store, ACTOR, now_ms());
    assert!(
        views
            .iter()
            .all(|view| view.generation == revised.generation)
    );
    assert!(
        views
            .iter()
            .any(|view| view.carried_claimant.as_deref() == Some(ACTOR))
    );
    // Equal owner pairs repeat across chunks, beside two valid generations of one run
    // and a missing owner for only one input. None and old-generation cancellation stay exact.
    let mut requested = vec![views[0].clone(); 257];
    requested[126] = carried.clone();
    requested[128] = handed.clone();
    requested[255].run = "mission-run/missing-owner".into();
    let at = now_ms();
    let connection = store.readers.get();
    let selected = owners(&connection, &requested).unwrap();
    assert!(selected[126].is_some() && selected[128].is_some());
    assert!(selected[255].is_none());
    for (view, owner) in requested.iter().zip(selected) {
        let mut old = view.clone();
        let mut new = view.clone();
        apply_effective_step_state(&connection, &mut old, at).unwrap();
        apply_effective_step_state_with_owner(&connection, &mut new, at, owner).unwrap();
        assert_eq!(
            serde_json::to_value(old).unwrap(),
            serde_json::to_value(new).unwrap()
        );
    }
    println!("real handoff and revision/carry parity seed={SEED:#x}");
}

#[test]
fn batched_work_metadata_snapshot_survives_second_writer_and_next_call_sees_claim() {
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(&root.path().join("claims.sqlite3"), "alder").unwrap());
    add_run(&store, 2, "batch-work");
    let before = parity(&store, ACTOR, now_ms());
    let original = store
        .readers
        .get()
        .query_row("SELECT body FROM mission_revisions", [], |r| {
            r.get::<_, String>(0)
        })
        .unwrap();
    let mut mission: MissionSpec = serde_json::from_str(&original).unwrap();
    for step in mission.steps.values_mut() {
        step.fresh_context = true;
    }
    let updated = serde_json::to_string(&mission).unwrap();
    let subject = before[0].subject.clone();
    let (enter, entered) = std::sync::mpsc::sync_channel(1);
    let (done, completed) = std::sync::mpsc::sync_channel(1);
    let writer_store = Arc::clone(&store);
    let writer = std::thread::spawn(move || {
        entered.recv().unwrap();
        {
            let connection = writer_store.connection.write();
            connection
                .execute(/* sqlite-large-value: allow-for-now lv-h212cdaa37d8a owner=st3::store::reconcile_work_context::tests TODO=bound-or-reference-values-and-reclamation */ "UPDATE mission_revisions SET body=?1", [updated])
                .unwrap();
            connection
                .execute(/* sqlite-large-value: allow-for-now lv-h0d85f35f0ae0 owner=st3::store::reconcile_work_context::tests TODO=bound-or-reference-values-and-reclamation */ "UPDATE mission_runs SET phase='revision-draining'", [])
                .unwrap();
        }
        // An actual admitted state claim also lands while the older read snapshot is pinned.
        writer_store
            .set_step_state(&subject, "completed", Some("Completed on a second thread"))
            .unwrap();
        done.send(()).unwrap();
    });
    AFTER_OWNERS.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || {
            enter.send(()).unwrap();
            completed
                .recv_timeout(std::time::Duration::from_secs(5))
                .expect("writer waited on the read snapshot");
        }))
    });
    let captured = read(&store, ACTOR, now_ms());
    writer.join().unwrap();
    assert_eq!(
        serde_json::to_value(captured.unwrap()).unwrap(),
        serde_json::to_value(&before).unwrap()
    );
    let after = parity(&store, ACTOR, now_ms());
    assert!(
        after.is_empty(),
        "new draining state drops every non-claimed step"
    );
    let state = store
        .latest_claim(&before[0].subject, Some("step-run.state"))
        .unwrap()
        .unwrap();
    assert_eq!(state.body["fields"]["status"], "completed");
}

fn declare_handoff_actors(store: &Store) {
    let agents = crate::graph::parse_internal_intent(
        "version 2\nagent \"alder.worker\" { workspace \"/tmp\"; command \"true\" }\nagent \"alder.peer\" { workspace \"/tmp\"; command \"true\" }", "alder").unwrap();
    store.apply_internal(&agents, "support-actors").unwrap();
}

fn claim_for_handoff(store: &Store, subject: &str) {
    store.set_step_state(subject, "ready", None).unwrap();
    store
        .work_action(
            subject,
            "claim",
            &WorkRequest {
                wait_ms: None,
                actor: Some(ACTOR.into()),
                incarnation: Some("support-worker-one".into()),
                summary: None,
                reason: None,
                evidence: Vec::new(),
                idempotency_key: "support-worker-claim".into(),
            },
        )
        .unwrap();
}

fn handoff_to_peer(store: &Store, subject: &str) {
    store
        .handoff_work(
            subject,
            &crate::model::WorkHandoffRequest {
                actor: ACTOR.into(),
                incarnation: Some("support-worker-one".into()),
                to: "agent/alder.peer".into(),
                note: "Continue this exact step.".into(),
                evidence: Vec::new(),
                idempotency_key: "support-transfer".into(),
            },
        )
        .unwrap();
}

#[test]
fn batched_work_support_keeps_handoff_attempt_clock_and_orphan_ack_inputs() {
    const PEER: &str = "agent/alder.peer";
    let store = fixture(2);
    declare_handoff_actors(&store);
    let initial = parity(&store, ACTOR, now_ms());
    assert!(
        support(&store.readers.get(), &initial)
            .unwrap()
            .iter()
            .all(|row| *row == (false, false))
    );
    let subject = &initial[SEED as usize % initial.len()].subject;
    claim_for_handoff(&store, subject);
    handoff_to_peer(&store, subject);
    let release = store
        .latest_claim(subject, Some("work.released"))
        .unwrap()
        .unwrap();
    let note = release.body["fields"]["handoff_message"].as_str().unwrap();
    store
        .acknowledge_work(
            subject,
            &crate::model::WorkAcknowledgeRequest {
                actor: PEER.into(),
                message: note.into(),
            },
        )
        .unwrap();
    // Reader-only edge fixtures retain the genuine transfer's bodies and positions. Their
    // timestamp/attempt variations exercise the original helper filters, not producer policy.
    let at = now_ms() + 10_000;
    store
        .connection
        .write()
        .execute(
            /* sqlite-large-value: allow-for-now lv-h8463284067c3 owner=st3::store::reconcile_work_context::tests TODO=prepare-body-and-predecessor-values */ "UPDATE claims SET accepted_at_unix_ms=?2 WHERE id=?1",
            params![release.id, (at + 1).to_string()],
        )
        .unwrap();
    let future = parity(&store, PEER, at);
    assert!(
        future
            .iter()
            .all(|v| !v.constraints.iter().any(|s| s.contains("Handoff from")))
    );
    store
        .connection
        .write()
        .execute(
            /* sqlite-large-value: allow-for-now lv-hccb3c321977b owner=st3::store::reconcile_work_context::tests TODO=prepare-body-and-predecessor-values */ "UPDATE claims SET accepted_at_unix_ms=?2 WHERE id=?1",
            params![release.id, at.to_string()],
        )
        .unwrap();
    store.connection.write().execute(
        /* sqlite-large-value: allow-for-now lv-hd599193b77e5 owner=st3::store::reconcile_work_context::tests TODO=prepare-body-and-predecessor-values */ "UPDATE claims SET accepted_at_unix_ms=?2 WHERE subject=?1 AND kind='work.progress' AND json_extract(body,'$.fields.handoff_acknowledged') IS NOT NULL",
        params![subject, (at + 1).to_string()],
    ).unwrap();
    let unacknowledged = parity(&store, PEER, at);
    assert!(unacknowledged.iter().any(|v| {
        v.constraints
            .iter()
            .any(|s| s.contains("awaiting acknowledgment"))
    }));
    let acknowledged = parity(&store, PEER, at + 1);
    assert!(
        acknowledged
            .iter()
            .any(|v| v.constraints.iter().any(|s| s.contains("(acknowledged)")))
    );
    // Expired lease and a release from another attempt keep the positive-presence path.
    store
        .connection
        .write()
        .execute(
            /* sqlite-large-value: allow-for-now lv-h8ea9b43a73ce owner=st3::store::reconcile_work_context::tests TODO=bound-or-reference-values-and-reclamation */ "UPDATE step_runs SET lease_expires_at_unix_ms=?2 WHERE subject=?1",
            params![subject, (at - 1).to_string()],
        )
        .unwrap();
    store
        .connection
        .write()
        .execute(
            /* sqlite-large-value: allow-for-now lv-h9f297a87f240 owner=st3::store::reconcile_work_context::tests TODO=prepare-body-and-predecessor-values */ "UPDATE claims SET body=json_set(body,'$.fields.attempt',?2) WHERE id=?1",
            params![release.id, initial[0].attempt + 1],
        )
        .unwrap();
    let stale = parity(&store, PEER, at + 1);
    assert!(
        stale
            .iter()
            .all(|v| !v.constraints.iter().any(|s| s.contains("Handoff from")))
    );
    assert!(
        support(&store.readers.get(), &stale)
            .unwrap()
            .iter()
            .any(|row| row.0)
    );
    // An orphan acknowledgment is admitted as a reader fixture on the OTHER step,
    // which never had a release. Keep the genuine transfer and its receipt references.
    let other = initial.iter().find(|v| &v.subject != subject).unwrap();
    store.connection.batched(|tx| -> Result<()> {
        append_claim_tx(tx, "alder", &other.subject, "work.progress", Some(PEER),
            &json!({"fields":{"attempt":other.attempt,"handoff_acknowledged":"message/no-transfer"}}), &[], None)?;
        Ok(())
    }).unwrap().unwrap();
    let orphan_ack = parity(&store, ACTOR, at + 1);
    let untouched = orphan_ack
        .iter()
        .find(|v| v.subject == other.subject)
        .unwrap();
    assert!(!support(&store.readers.get(), std::slice::from_ref(untouched)).unwrap()[0].0);
    assert!(
        untouched
            .constraints
            .iter()
            .all(|s| !s.contains("Handoff from"))
    );
}

#[test]
fn batched_work_support_snapshot_keeps_absence_across_a_second_writer_handoff() {
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(&root.path().join("claims.sqlite3"), "alder").unwrap());
    add_run(&store, 2, "batch-work");
    declare_handoff_actors(&store);
    let initial = parity(&store, ACTOR, now_ms());
    claim_for_handoff(&store, &initial[0].subject);
    let at = now_ms();
    let before = parity(&store, ACTOR, at);
    assert!(
        support(&store.readers.get(), &before)
            .unwrap()
            .iter()
            .all(|row| !row.0)
    );
    let subject = initial[0].subject.clone();
    let (enter, entered) = std::sync::mpsc::sync_channel(1);
    let (done, completed) = std::sync::mpsc::sync_channel(1);
    let writer_store = Arc::clone(&store);
    let writer = std::thread::spawn(move || {
        entered.recv().unwrap();
        // Normal admitted transfer on the Store's writer connection, distinct from this
        // thread's pinned reader. No authority/effect decision uses the absence proof.
        handoff_to_peer(&writer_store, &subject);
        done.send(()).unwrap();
    });
    AFTER_SUPPORT.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || {
            enter.send(()).unwrap();
            completed
                .recv_timeout(std::time::Duration::from_secs(5))
                .expect("writer waited on pinned support proof");
        }));
    });
    let captured = read(&store, ACTOR, at);
    writer.join().unwrap();
    assert_eq!(
        serde_json::to_value(captured.unwrap()).unwrap(),
        serde_json::to_value(&before).unwrap()
    );
    let next = parity(&store, ACTOR, now_ms());
    assert_eq!(next.len(), before.len() - 1);
    let recipient = parity(&store, "agent/alder.peer", now_ms());
    assert!(
        recipient
            .iter()
            .any(|v| v.constraints.iter().any(|s| s.contains("Handoff from")))
    );
}

#[test]
fn batched_work_support_query_errors_refuse_the_whole_read() {
    let store = fixture(1);
    parity(&store, ACTOR, now_ms());
    // The existing schema installs this index before opening readers. A corrupt schema
    // is a hard prepare failure, never a waiver or a fallback to a table scan/helper skip.
    store
        .connection
        .write()
        .execute(/* sqlite-large-value: allow-for-now lv-hc0c4e85b8a3b owner=st3::store::reconcile_work_context::tests TODO=bound-or-reference-values-and-reclamation */ "DROP INDEX claims_person_ask_origin_step_index", [])
        .unwrap();
    let error = read(&store, ACTOR, now_ms()).unwrap_err();
    assert!(error.to_string().contains("no such index"), "{error}");
}

#[test]
fn batched_work_support_cost_reports_empty_and_all_present_4200_step_mixes() {
    let store = fixture(4200);
    let at = now_ms();
    let base = read(&store, ACTOR, at).unwrap();
    let connection = store.readers.get();
    let legacy_helpers = || {
        let mut views = base.clone();
        for view in &mut views {
            adhoc_work::enrich_handoff(&connection, view, at).unwrap();
            person_work::enrich_responses(&connection, view).unwrap();
        }
        views
    };
    let with_proof = || {
        let flags = support(&connection, &base).unwrap();
        let mut views = base.clone();
        for (view, (release, ask)) in views.iter_mut().zip(flags) {
            if release {
                adhoc_work::enrich_handoff(&connection, view, at).unwrap();
            }
            if ask {
                person_work::enrich_responses(&connection, view).unwrap();
            }
        }
        views
    };
    // Warm each path outside scopes. These counters isolate the proof and the two
    // unchanged helpers; the separate whole-read control includes owners and definitions.
    legacy_helpers();
    with_proof();
    support(&connection, &base).unwrap();
    let (empty_old, old_work, old_time) = measured(legacy_helpers);
    let (empty_new, new_work, new_time) = measured(with_proof);
    let (_, proof_work, proof_time) = measured(|| support(&connection, &base).unwrap());
    println!(
        "4200-step EMPTY helper mix: old={old_work:?} {old_time:?}; proof-only={proof_work:?} {proof_time:?}; proof+helpers={new_work:?} {new_time:?}"
    );
    assert_eq!(
        serde_json::to_value(empty_old).unwrap(),
        serde_json::to_value(empty_new).unwrap()
    );
    assert_eq!(old_work.statements, 8400);
    assert_eq!(proof_work.statements, 4200_usize.div_ceil(CHUNK) as u64);
    assert_eq!(new_work, proof_work);
    assert!(proof_work.vm_steps < old_work.vm_steps);
    assert_eq!(proof_work.fullscan_steps, 0);
    drop(connection);
    // Isolated presence fixtures append actual claim rows without projecting their
    // lifecycle. Every step has both kinds; releases have no handoff note and requests
    // have no resolution, so this measures the proof's pure overhead when it skips none.
    store.connection.batched(|tx| -> Result<()> {
        for view in &base {
            append_claim_tx(tx, "alder", &view.subject, "work.released", Some(ACTOR),
                &json!({"fields":{"attempt":view.attempt,"status":"ready","summary":"Released"}}), &[], None)?;
            append_claim_tx(tx, "alder", &view.subject, "work.person-asked", Some(ACTOR),
                &json!({"fields":{"origin_step":view.subject,"origin_attempt":view.attempt}}), &[], None)?;
        }
        Ok(())
    }).unwrap().unwrap();
    let connection = store.readers.get();
    let flags = support(&connection, &base).unwrap();
    assert!(flags.iter().all(|row| *row == (true, true)));
    let legacy_helpers = || {
        let mut views = base.clone();
        for view in &mut views {
            adhoc_work::enrich_handoff(&connection, view, at).unwrap();
            person_work::enrich_responses(&connection, view).unwrap();
        }
        views
    };
    let with_proof = || {
        let flags = support(&connection, &base).unwrap();
        let mut views = base.clone();
        for (view, (release, ask)) in views.iter_mut().zip(flags) {
            if release {
                adhoc_work::enrich_handoff(&connection, view, at).unwrap();
            }
            if ask {
                person_work::enrich_responses(&connection, view).unwrap();
            }
        }
        views
    };
    legacy_helpers();
    with_proof();
    support(&connection, &base).unwrap();
    let (present_old, old_work, old_time) = measured(legacy_helpers);
    let (present_new, new_work, new_time) = measured(with_proof);
    let (_, proof_work, proof_time) = measured(|| support(&connection, &base).unwrap());
    println!(
        "4200-step ALL-PRESENT helper mix: old={old_work:?} {old_time:?}; proof-only={proof_work:?} {proof_time:?}; proof+helpers={new_work:?} {new_time:?}; added VM={} statements={}",
        new_work.vm_steps - old_work.vm_steps,
        new_work.statements - old_work.statements
    );
    assert_eq!(
        serde_json::to_value(present_old).unwrap(),
        serde_json::to_value(present_new).unwrap()
    );
    assert_eq!(old_work.statements, 8400);
    assert_eq!(
        new_work.statements,
        old_work.statements + proof_work.statements
    );
    assert_eq!(new_work.vm_steps, old_work.vm_steps + proof_work.vm_steps);
    assert_eq!(proof_work.fullscan_steps, 0);
}

#[test]
fn batched_work_definition_index_preserves_nested_display_order_and_first_match() {
    let store = fixture(3);
    let body: String = store
        .readers
        .get()
        .query_row("SELECT body FROM mission_revisions", [], |r| r.get(0))
        .unwrap();
    let mut mission: MissionSpec = serde_json::from_str(&body).unwrap();
    let first = mission.display_order[0].clone();
    let second = mission.display_order[1].clone();
    let third = mission.display_order[2].clone();
    let mut nested = mission.clone();
    nested.display_order = vec![second.clone(), third.clone()];
    nested.steps.get_mut(&second).unwrap().path = mission.steps[&first].path.clone();
    nested.steps.get_mut(&second).unwrap().queue = Some("nested-duplicate".into());
    nested.steps.get_mut(&third).unwrap().path = "parent/nested-child".into();
    nested.steps.get_mut(&third).unwrap().queue_position = Some(7);
    nested.steps.get_mut(&third).unwrap().timeout_ms = Some(31);
    nested.steps.get_mut(&third).unwrap().fresh_context = true;
    mission.steps.get_mut(&first).unwrap().queue = Some("parent-first".into());
    mission.steps.get_mut(&first).unwrap().nested_mission = Some(Box::new(nested));
    // Root second also matches a nested path, but depth-first nested entry wins.
    mission.steps.get_mut(&second).unwrap().path = "parent/nested-child".into();
    mission.steps.get_mut(&second).unwrap().timeout_ms = Some(99);
    let index = DefinitionIndex::from_mission(&mission);
    for path in [
        mission.steps[&first].path.as_str(),
        "parent/nested-child",
        mission.steps[&third].path.as_str(),
        "not-present",
    ] {
        let old = crate::mission::find_step(&mission, path);
        let new = index.get(path).unwrap();
        assert_eq!(new.is_some(), old.is_some());
        if let (Some(old), Some(new)) = (old, new) {
            assert_eq!(new.queue, old.queue);
            assert_eq!(new.queue_position, old.queue_position);
            assert_eq!(new.timeout_ms, old.timeout_ms);
            assert_eq!(new.fresh_context, old.fresh_context);
        }
    }
    // Semantic corruption of a definition: preserve matches before the broken reference,
    // and refuse unresolved lookups beyond it. No unrelated default silently fills a row.
    mission.display_order = vec![
        first.clone(),
        "missing-definition-step".into(),
        third.clone(),
    ];
    let index = DefinitionIndex::from_mission(&mission);
    assert_eq!(
        index
            .get(&mission.steps[&first].path)
            .unwrap()
            .unwrap()
            .queue,
        Some("parent-first".into())
    );
    assert!(index.get(&mission.steps[&third].path).is_err());
    assert!(index.get("not-present").is_err());
    store
        .connection
        .write()
        .execute(
            /* sqlite-large-value: allow-for-now lv-h9a7515c8a758 owner=st3::store::reconcile_work_context::tests TODO=bound-or-reference-values-and-reclamation */ "UPDATE mission_revisions SET body=?1",
            [serde_json::to_string(&mission).unwrap()],
        )
        .unwrap();
    let error = read(&store, ACTOR, now_ms()).unwrap_err();
    assert!(
        error.to_string().contains("missing-definition-step"),
        "{error}"
    );
}

#[test]
fn batched_work_metadata_registers_candidates_before_visibility_filtering() {
    let store = fixture(129);
    let at = now_ms();
    let candidates = parity(&store, ACTOR, at);
    assert_eq!(candidates.len(), 129);
    store
        .connection
        .write()
        .execute(/* sqlite-large-value: allow-for-now lv-h8b17c104c16a owner=st3::store::reconcile_work_context::tests TODO=bound-or-reference-values-and-reclamation */ "UPDATE mission_runs SET phase='revision-draining'", [])
        .unwrap();
    let (old, old_keys) = smallclaims::touched::record(|| oracle(&store, ACTOR, at).unwrap());
    let (new, new_keys) = smallclaims::touched::record(|| read(&store, ACTOR, at).unwrap());
    assert!(old.is_empty());
    assert!(new.is_empty());
    assert_eq!(
        new_keys, old_keys,
        "filtered candidates retain the same dependencies"
    );
    for candidate in &candidates {
        for key in [&candidate.subject, &candidate.run, &candidate.generation] {
            assert!(new_keys.contains(key), "filtered candidate lost {key}");
        }
    }
}

#[test]
fn batched_work_metadata_origin_ask_after_a_pass_invalidates_its_seat() {
    use crate::incremental::{Incremental, change_keys};
    const OTHER: &str = "agent/alder.unrelated";
    let (store, origin, input) = person_work::tests::fixture();
    let feed = Incremental::default();
    let ask_only = Incremental::default();
    feed.observe(&store).unwrap();
    for incremental in [&feed, &ask_only] {
        for actor in [&input.actor[..], OTHER] {
            let (_, keys) =
                smallclaims::touched::record(|| store.work_for_reconcile(actor).unwrap());
            incremental.evaluated(actor, keys, None);
            assert!(!incremental.needs(actor, 0));
        }
    }
    // Record key parity before an ask exists, while the batched support proof skips
    // the empty response reader. The writer below is the real claimant/ask path.
    parity(&store, &input.actor, now_ms());
    let ask = store.ask_person(&input).unwrap();
    assert_ne!(ask.subject, origin.subject);
    let request = store
        .latest_claim(&ask.subject, Some("work.person-asked"))
        .unwrap()
        .unwrap();
    assert_eq!(request.body["fields"]["origin_step"], origin.subject);
    let change = Change {
        subject: request.subject.clone(),
        kind: request.kind.clone(),
        actor: request.actor.clone(),
        body: serde_json::to_string(&request.body).unwrap(),
    };
    // Isolate the ask claim's own keys: do not rely on its companion origin
    // step-run.state receipt to make this assertion pass. The claimant actor key
    // already belongs to this seat's public work read.
    let keys = change_keys(&change);
    assert!(keys.contains(&format!("actor:{}", input.actor)));
    for key in keys {
        ask_only.touch(&key);
    }
    assert!(ask_only.needs(&input.actor, 0));
    assert!(!ask_only.needs(OTHER, 0));
    feed.observe(&store).unwrap();
    assert!(
        feed.needs(&input.actor, 0),
        "the actual change feed must dirty the settled seat"
    );
    assert!(!feed.needs(OTHER, 0));
    let waiting = parity(&store, &input.actor, now_ms());
    assert!(
        waiting
            .iter()
            .any(|view| view.subject == origin.subject && view.status == "waiting-person")
    );
    let (_, keys) =
        smallclaims::touched::record(|| store.work_for_reconcile(&input.actor).unwrap());
    feed.evaluated(&input.actor, keys, None);
    assert!(!feed.needs(&input.actor, 0));
    store
        .finish_person_step(
            &crate::model::PersonStepResponse {
                delegation: None,
                subject: ask.subject,
                actor: input.person.clone(),
                summary: "Friday".into(),
                evidence: Vec::new(),
                episode: None,
                answer: None,
                idempotency_key: "dependency-person-done".into(),
            },
            false,
        )
        .unwrap();
    feed.observe(&store).unwrap();
    assert!(
        feed.needs(&input.actor, 0),
        "the real answer path must dirty the waiting seat"
    );
    assert!(!feed.needs(OTHER, 0));
    let answered = parity(&store, &input.actor, now_ms());
    assert!(
        answered
            .iter()
            .any(|view| view.subject == origin.subject && !view.person_answers.is_empty())
    );
}
