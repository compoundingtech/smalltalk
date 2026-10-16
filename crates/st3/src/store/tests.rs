use super::*;
use crate::graph::parse_test_intent as parse_intent;
use proptest::prelude::*;

mod resource_pending_receive_tests {
    use super::*;
    include!("resource_pending_receive_tests.rs");
}

mod terminal_projection_date_tests {
    use super::*;
    include!("terminal_projection_date_tests.rs");
}

mod mission_eval_reuse_tests {
    include!("mission_eval_reuse_tests.rs");
}

const TEST_FLEET: &str = "018f6f0d-4a5d-7b8c-9d0e-123456789abc";

mod mission_deadline_probe {
    use super::*;
    include!("mission_deadline_probe.rs");
}

mod stopped_seat_declaration_controls {
    use super::*;
    include!("stopped_seat_declaration_controls.rs");
}

mod mission_deadline_tests {
    use super::*;
    include!("mission_deadline_tests.rs");
}

#[test]
fn an_agent_card_build_does_not_hold_other_read_snapshots_at_the_cache_lock() {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(&directory.path().join("graph.db"), "node").unwrap());
    store
        .cached_agent_resources(0, true, |_| Ok(vec![json!({"id": "agent/cached"})]))
        .unwrap();
    let (entered, building) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel();
    let slow = store.clone();
    let builder = std::thread::spawn(move || {
        slow.read_snapshot(|_| {
            slow.cached_agent_resources(1, false, |_| {
                entered.send(()).unwrap();
                released.recv().unwrap();
                Ok(vec![json!({"id": "agent/slow"})])
            })
        })
        .unwrap()
    });
    building.recv().unwrap();
    let (finished, result) = std::sync::mpsc::channel();
    let reader = store.clone();
    let other = std::thread::spawn(move || {
        finished
            .send(reader.read_snapshot(|_| {
                reader.cached_agent_resources(0, true, |_| panic!("cached snapshot was lost"))
            }))
            .unwrap();
    });
    let completed = result.recv_timeout(std::time::Duration::from_secs(1));
    let published = completed.as_ref().ok().map(|_| {
        store
            .cached_agent_resources(1, false, |_| Ok(vec![json!({"id": "agent/published"})]))
            .unwrap()
    });
    release.send(()).unwrap();
    let resumed = builder.join().unwrap();
    other.join().unwrap();
    assert_eq!(completed.unwrap().unwrap()[0]["id"], "agent/cached");
    assert_eq!(resumed, published.unwrap());
}

#[test]
fn projection_times_at_names_every_cut_like_the_single_lookup() {
    let store = roster_cache_store();
    let (_, newest) = store.event_bounds().unwrap();
    assert!(newest > 1, "the fixture needs several claims");
    // Heterogeneous, repeated, unordered, empty-store and past-the-frontier cuts.
    let mut cuts = vec![0, newest + 7, 1, newest, 1, newest / 2, 0, newest];
    cuts.extend(1..=newest);
    let batched = store.projection_times_at(&cuts).unwrap();
    for cut in &cuts {
        assert_eq!(batched[cut], store.projection_time_at(*cut).unwrap(), "cut {cut}");
    }
    assert_eq!(batched.len(), (0..=newest + 7).filter(|c| cuts.contains(c)).count());
    assert!(store.projection_times_at(&[]).unwrap().is_empty());
    // Acceptance times are per claim, not one shared value, once the clock moves.
    assert!(batched[&newest] >= batched[&1]);
}

#[test]
fn projection_times_at_seeks_the_claim_index_once_per_cut() {
    let store = roster_cache_store();
    let connection = store.readers.get();
    let plan = connection
        .prepare(
            "EXPLAIN QUERY PLAN SELECT cut.value,
                        (SELECT accepted_at_unix_ms FROM claims
                         WHERE store_index <= cut.value ORDER BY store_index DESC LIMIT 1)
                 FROM json_each(?1) AS cut",
        )
        .unwrap()
        .query_map(["[1,2]"], |row| row.get::<_, String>(3))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
        .join("\n");
    assert!(plan.contains("SEARCH claims USING INTEGER PRIMARY KEY"), "{plan}");
    assert!(!plan.contains("SCAN claims"), "{plan}");
}

fn roster_cache_store() -> Store {
    let store = Store::open_memory("node").unwrap();
    let source = "version 2\nagent \"amber\" { command \"true\" }\n";
    let intent = parse_intent(source, "node").unwrap();
    let plan = store
        .mission(
            &intent,
            crate::model::IntentInput {
                kdl: source.into(),
                source_name: None,
            },
        )
        .unwrap();
    store.apply(&intent, &plan.subject_tokens, "roster-cache").unwrap();
    store.append_claim(&local_observation_for_test("harness.observed", json!({
        "state":"idle", "driver":"codex", "incarnation_id":"amber-1", "observed_at_ms":1,
    }))).unwrap();
    store
}

fn local_observation_for_test(kind: &str, fields: Value) -> ClaimInput {
    ClaimInput {
        subject: "agent/node.amber".into(),
        kind: kind.into(),
        actor: Some("agent/node.amber".into()),
        fields: serde_json::from_value(fields).unwrap(),
        evidence: Vec::new(),
        expected_subject: None,
        idempotency_key: None,
    }
}

fn local_timeline_observation_for_test() -> ClaimInput {
    local_observation_for_test("harness.timeline", json!({
        "operation":"append", "entry_id":"local-activity", "source_id":"fixture/local-activity",
        "sequence":1, "revision":1, "role":"assistant", "entry_type":"content", "final":true,
        "driver":"codex", "incarnation_id":"amber-1",
        "observed_at_unix_ms":1_900_000_000_000_u64, "body":{"text":"local activity"},
    }))
}

/// A heartbeat moves no card, so heartbeat-only local deltas keep pinning the same roster
/// rows; an agent timeline row is the local delta a card reads.
#[test]
fn heartbeat_local_deltas_pin_the_same_roster_rows() {
    let store = roster_cache_store();
    let index = store.index().unwrap();
    let card = json!({"id": "agent/node.amber", "name": "amber"});
    store
        .cached_agent_resources(index, false, |_| Ok(vec![card.clone()]))
        .unwrap();
    let pinned = store
        .agent_resources_cached_at(index, false, None)
        .unwrap()
        .unwrap();
    for n in 0..8 {
        let response = store.append_claim(&local_observation_for_test(
            "harness.observed",
            json!({"state":"idle", "driver":"codex", "incarnation_id":"amber-1",
                "observed_at_ms":300_001 + n}),
        )).unwrap();
        assert!(local_observation_position(&response).is_some());
        assert_eq!(
            store.index().unwrap(),
            index,
            "a local observation keeps the graph cut"
        );
        let hit = store
            .agent_resources_cached_at(index, false, None)
            .unwrap()
            .expect("a heartbeat moves no card, so the pinned rows still answer");
        assert!(Arc::ptr_eq(&hit, &pinned), "heartbeat deltas must reuse the roster rows");
    }
    assert_eq!(store.agent_resources_builds_for_test(), 1);
    store.append_local_observations_for_test(&[local_timeline_observation_for_test()]);
    assert_eq!(store.index().unwrap(), index);
    assert!(
        store
            .agent_resources_cached_at(index, false, None)
            .unwrap()
            .is_none(),
        "an agent timeline row is the local delta a card reads",
    );
    store
        .cached_agent_resources(index, false, |_| Ok(vec![card]))
        .unwrap();
    assert!(
        store
            .agent_resources_cached_at(index, false, None)
            .unwrap()
            .is_some()
    );
    assert_eq!(store.agent_resources_builds_for_test(), 2);
}

/// A page that already covered every agent at the cut upgrades to the complete window by
/// reusing the pinned rows: no clone, no sort, no build.
#[test]
fn a_covered_page_upgrade_reuses_the_pinned_roster_rows() {
    let store = roster_cache_store();
    let index = store.index().unwrap();
    let selected = BTreeSet::from(["agent/node.amber".to_string()]);
    store
        .cached_agent_resources_for(index, false, Some(&selected), |_| {
            Ok(vec![json!({"id": "agent/node.amber", "name": "amber"})])
        })
        .unwrap();
    let partial = store
        .agent_resources_cached_at(index, false, Some(&selected))
        .unwrap()
        .unwrap();
    store
        .cached_agent_resources_for(index, false, None, |changed| {
            panic!("nothing moved and nothing is uncovered, so no card is built: {changed:?}")
        })
        .unwrap();
    let complete = store
        .agent_resources_cached_at(index, false, None)
        .unwrap()
        .unwrap();
    assert!(Arc::ptr_eq(&complete, &partial));
    assert_eq!(store.agent_resources_builds_for_test(), 1);
}

/// The frontier answers an old cut exactly — its newest agent timeline row — from the
/// partial frontier index, with VM work that newer heartbeats never grow; the pre-followup
/// lookup read every newer local row of every kind. Focused fixture: run it explicitly and
/// alone, with --ignored --nocapture, because its counters are process-wide.
#[test]
#[ignore = "focused roster frontier fixture; run explicitly with --ignored --nocapture"]
fn the_roster_frontier_old_cut_seek_skips_newer_heartbeats() {
    let store = roster_cache_store();
    let cut = store.index().unwrap();
    store
        .append_local_observations_for_test(&[local_timeline_observation_for_test()]);
    let relevant_id = store
        .readers
        .get()
        .query_row(
            "SELECT id FROM local_observations ORDER BY id DESC LIMIT 1",
            [],
            |row| row.get::<_, u64>(0),
        )
        .unwrap();

    let later = insert_raw_claims(&store, &[("host/one", "transport.observed")]);
    let insert_newer = |heartbeats: usize, irrelevant: usize, usage: usize| {
        let connection = store.connection.lock().unwrap();
        let mut statement = connection
            .prepare(
                "INSERT INTO local_observations(after_store_index, subject, kind, body,
                                                        observed_at_unix_ms)
                     VALUES (?1, ?2, ?3, '{}', 1)",
            )
            .unwrap();
        for _ in 0..heartbeats {
            statement
                .execute(params![later, "agent/node.amber", "harness.observed"])
                .unwrap();
        }
        for _ in 0..irrelevant {
            statement
                .execute(params![later, "mission-run/x", "harness.timeline"])
                .unwrap();
        }
        for _ in 0..usage {
            statement
                .execute(params![later, "agent/node.amber", "harness.timeline"])
                .unwrap();
        }
    };
    insert_newer(4_000, 500, 200);

    let frontier = |at: u64| {
        store
            .readers
            .get()
            .query_row(ROSTER_LOCAL_FRONTIER, [at], |row| row.get::<_, u64>(0))
            .unwrap()
    };
    assert_eq!(
        frontier(cut),
        relevant_id,
        "an old cut sees exactly its newest agent timeline row"
    );
    let usage_frontier = store
        .readers
        .get()
        .query_row(
            "SELECT id FROM local_observations WHERE subject='agent/node.amber'
                 AND kind='harness.timeline' ORDER BY id DESC LIMIT 1",
            [],
            |row| row.get::<_, u64>(0),
        )
        .unwrap();
    assert_eq!(
        frontier(later),
        usage_frontier,
        "the later cut sees the newest agent timeline row, of any entry type"
    );

    let plan = query_plan(&store, ROSTER_LOCAL_FRONTIER, params![cut]);
    assert!(
        plan.iter()
            .any(|step| step.contains("local_observations_roster_frontier_index")),
        "{plan:?}"
    );
    assert!(plan.iter().all(|step| !step.contains("TEMP B-TREE")), "{plan:?}");

    let pre_followup = "SELECT COALESCE((SELECT id FROM local_observations
             WHERE after_store_index<=?1 ORDER BY id DESC LIMIT 1), 0)";
    let vm_steps = |sql: &str, at: u64| {
        let connection = store.connection.lock().unwrap();
        let _: u64 = connection.query_row(sql, [at], |row| row.get(0)).unwrap();
        let before = smallclaims::sqlite::work::total().vm_steps;
        let _: u64 = connection.query_row(sql, [at], |row| row.get(0)).unwrap();
        drop(connection);
        smallclaims::sqlite::work::total().vm_steps - before
    };
    let old_before = vm_steps(pre_followup, cut);
    let new_before = vm_steps(ROSTER_LOCAL_FRONTIER, cut);
    insert_newer(4_000, 0, 0);
    let old_after = vm_steps(pre_followup, cut);
    let new_after = vm_steps(ROSTER_LOCAL_FRONTIER, cut);
    println!(
        "roster frontier fixture: heartbeats=4000..8000 irrelevant_timeline=500 \
             agent_usage_timeline=200 vm_old_cut_before={old_before} \
             vm_new_cut_before={new_before} vm_old_cut_after={old_after} \
             vm_new_cut_after={new_after}"
    );
    assert_eq!(
        new_after, new_before,
        "the frontier seek's work cannot grow with newer heartbeat rows"
    );
    assert!(
        old_after > old_before,
        "the pre-followup lookup read every newer row of every kind"
    );
    assert!(
        new_before < old_before,
        "the frontier index bounds the old-cut lookup"
    );
}

#[test]
fn terminal_capability_lookup_fences_the_subject_head_after_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("graph.db");
    let store = Store::open(&path, "node").unwrap();
    let append = |subject: &str, kind: &str| {
        store.append_claim(&ClaimInput {
            subject: subject.into(),
            kind: kind.into(),
            actor: None,
            fields: BTreeMap::from([("capability_hash".into(), json!("secret-digest"))]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        }).unwrap()
    };
    let attached = append("custom/client/attachment-a", "custom.client.terminal-attached");
    append("custom/client/decoy", "custom.client.other");
    assert_eq!(
        store.terminal_attachment_for_capability_hash("secret-digest").unwrap()
            .map(|(claim, current)| (claim.id, current)),
        Some((attached.id.clone(), true)),
    );
    assert!(store.terminal_attachment_for_capability_hash("unknown").unwrap().is_none());
    append("custom/client/attachment-a", "custom.client.terminal-consumed");
    drop(store);
    let reopened = Store::open(&path, "node").unwrap();
    assert_eq!(
        reopened.terminal_attachment_for_capability_hash("secret-digest").unwrap()
            .map(|(claim, current)| (claim.id, current)),
        Some((attached.id, false)),
    );
}

mod mailbox_fence_tests {
    use super::*;
    include!("mailbox_fence_tests.rs");
}

mod canonical_audit {
    use super::*;
    include!("canonical_audit.rs");
}

mod declarative_revision_timestamp_tests {
    use super::*;
    include!("declarative_revision_timestamp_tests.rs");
}

mod actual_purity_tests {
    use super::*;
    include!("actual_purity_tests.rs");
}

mod activity_purity_tests {
    use super::*;
    include!("activity_purity_tests.rs");
}

mod fault_history_tests {
    use super::*;
    include!("fault_history_tests.rs");
}

mod projection_divergence_tests {
    use super::*;
    include!("projection_divergence_tests.rs");
}

#[test]
fn seat_rename_persists_across_a_real_store_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.sqlite3");
    let original = {
        let store = Store::open(&path, "node").unwrap();
        let intent = parse_intent(r#"version 2
agent "test/worker" { workspace "."; command "true"; name "Initial" }
"#, "node").unwrap();
        let preview = store.mission(&intent, IntentInput {
            kdl: String::new(), source_name: None,
        }).unwrap();
        store.apply_as(&intent, &preview.subject_tokens, "initial", Some("person/operator")).unwrap();
        let original = store.desired_subject_with_writer("agent/test/worker").unwrap().unwrap();
        store.rename_agent("agent/test/worker", Some("Renamed"), "rename").unwrap();
        original
    };
    let store = Store::open(&path, "node").unwrap();
    let (mut renamed, writer) = store.desired_subject_with_writer("agent/test/worker").unwrap().unwrap();
    assert_eq!(writer, original.1);
    assert_eq!(renamed.member.as_ref().unwrap().display_name.as_deref(), Some("Renamed"));
    renamed.set_display_name(Some("Initial")).unwrap();
    assert_eq!(renamed, original.0);
    store.rename_agent("agent/test/worker", None, "clear").unwrap();
    drop(store);
    let store = Store::open(&path, "node").unwrap();
    let cleared = store.desired_subject_with_writer("agent/test/worker").unwrap().unwrap().0;
    assert_eq!(crate::model::effective_agent_name("agent/test/worker", Some(&cleared.desired)), "test/worker");
    assert_eq!(cleared.member.unwrap().display_name, None);
}

#[test]
fn declared_launch_lineage_fences_subject_kind_and_launch_changes() {
    let original = parse_intent(
        r#"version 2
    agent "garden/orchard" { command "true" }
    "#,
        "amber",
    )
    .unwrap()
    .subjects
    .remove("agent/garden/orchard")
    .unwrap();
    let previous = serde_json::to_value(&original).unwrap();
    for edit in [r#"env { COLOR "blue"; }"#, r#"name "New label";"#] {
        let next = parse_intent(
            &format!("version 2\nagent \"garden/orchard\" {{ command \"true\"; {edit} }}"),
            "amber",
        )
        .unwrap();
        assert!(same_declared_launch(
            &previous,
            &serde_json::to_value(&next.subjects["agent/garden/orchard"]).unwrap()
        ));
    }
    for field in ["subject", "kind", "launch"] {
        let mut next = original.clone();
        match field {
            "subject" => next.subject = "agent/garden/meadow".into(),
            "kind" => next.kind = "stop".into(),
            "launch" => {
                next.member.as_mut().unwrap().launch =
                    crate::model::LaunchSpec::Shell("false".into())
            }
            _ => unreachable!(),
        }
        assert!(
            !same_declared_launch(&previous, &serde_json::to_value(&next).unwrap()),
            "{field}"
        );
    }
    assert!(!same_declared_launch(&previous, &json!({"kind":"agent"})));
}

#[test]
fn a_rename_that_merges_concurrent_renames_keeps_the_launch_revision() {
    let left = Store::open_memory("left").unwrap();
    let right = Store::open_memory("right").unwrap();
    let intent = parse_intent(r#"version 2
agent "test/worker" { command "true"; name "A" }
"#, "left").unwrap();
    let preview = left.mission(&intent, IntentInput {
        kdl: String::new(), source_name: None,
    }).unwrap();
    left.apply(&intent, &preview.subject_tokens, "a").unwrap();
    let launch = left.selected_desired_token("agent/test/worker").unwrap().unwrap();
    receive_and_project(&right, "left", &exchange_from(&left, &ReplicationInventory::default()));
    // Two hosts relabel the same seat concurrently, then one relabels the merged fork.
    left.rename_agent("agent/test/worker", Some("R1"), "r1").unwrap();
    right.rename_agent("agent/test/worker", Some("R2"), "r2").unwrap();
    receive_and_project(&right, "left", &exchange_from(&left, &right.replication_inventory().unwrap()));
    receive_and_project(&left, "right", &exchange_from(&right, &left.replication_inventory().unwrap()));
    left.rename_agent("agent/test/worker", Some("R3"), "r3").unwrap();
    let merged = left.selected_desired_token("agent/test/worker").unwrap().unwrap();
    assert_eq!(left.claim_by_id(&merged).unwrap().unwrap().predecessors.len(), 2);
    let lineage = left.launch_lineage("agent/test/worker").unwrap();
    assert_eq!(lineage.first(), Some(&merged));
    assert_eq!(lineage.last(), Some(&launch), "{lineage:?}");
}

#[test]
fn seat_rename_refuses_an_unreadable_member_and_an_empty_label() {
    let store = Store::open_memory("node").unwrap();
    let intent = parse_intent(r#"version 2
agent "test/worker" { command "true"; name "Initial" }
"#, "node").unwrap();
    let preview = store.mission(&intent, IntentInput {
        kdl: String::new(), source_name: None,
    }).unwrap();
    store.apply(&intent, &preview.subject_tokens, "initial").unwrap();
    let before = store.selected_desired_token("agent/test/worker").unwrap();
    assert_eq!(
        store.rename_agent("agent/test/worker", Some(""), "empty").unwrap_err().code,
        "invalid-agent-name"
    );
    // A launch published by a newer peer stays until a build that can read it takes it up.
    store.replace_desired_member_for_test("agent/test/worker", r#"{"future":true}"#);
    assert_eq!(
        store.rename_agent("agent/test/worker", Some("Renamed"), "unreadable").unwrap_err().code,
        "unreadable-agent-member"
    );
    assert_eq!(store.selected_desired_token("agent/test/worker").unwrap(), before);
    let connection = store.readers.get();
    let member: String = connection
        .query_row("SELECT member FROM desired WHERE subject='agent/test/worker'", [], |row| row.get(0))
        .unwrap();
    assert_eq!(member, r#"{"future":true}"#);
}

#[test]
fn seat_rename_accepts_an_empty_body_and_rejects_a_non_array_body() {
    let store = Store::open_memory("node").unwrap();
    let mut intent = parse_intent(r#"version 2
agent "test/empty" { command "true" }
"#, "node").unwrap();
    // An empty canonical KDL body has no children key. Exercise that stored shape
    // independently of launch validation, which requires a command for new seats.
    let empty = intent.subjects.get_mut("agent/test/empty").unwrap();
    empty.desired.as_object_mut().unwrap().remove("children");
    empty.member = None;
    let preview = store.mission(&intent, IntentInput {
        kdl: String::new(), source_name: None,
    }).unwrap();
    store.apply(&intent, &preview.subject_tokens, "empty").unwrap();
    store.rename_agent("agent/test/empty", Some("Empty seat"), "rename-empty").unwrap();
    let mut desired = store.desired_subject_with_writer("agent/test/empty").unwrap().unwrap().0;
    assert_eq!(crate::model::effective_agent_name("agent/test/empty", Some(&desired.desired)), "Empty seat");
    desired.desired["children"] = json!({});
    assert_eq!(desired.set_display_name(Some("Invalid")).unwrap_err().code, "invalid-agent-declaration");
}

#[test]
fn subject_claim_pages_seek_the_subject_index() {
    let store = Store::open_memory("node").unwrap();
    store.claims_page(None, None, 0, None, false, 1).unwrap();
    store
        .claims_page(Some("agent/target"), None, 0, None, false, 1)
        .unwrap();
    let connection = store.readers.get();
    for descending in [false, true] {
        let query = format!("EXPLAIN QUERY PLAN {}", claims_page_query(true, descending));
        let plan = connection
            .prepare(&query)
            .unwrap()
            .query_map(params![0, None::<u64>, "agent/target", 2], |row| {
                row.get::<_, String>(3)
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
            .join("; ");
        assert!(
            plan.contains("claims_subject_index"),
            "a subject page must not scan the whole graph: {plan}"
        );
    }
}

#[test]
fn a_pinned_read_sees_one_snapshot_while_commits_land() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(&directory.path().join("state.sqlite3"), "node").unwrap();
    let observe = |n: u64| {
        store
            .append_claim(&ClaimInput {
                subject: "resource/pinned".into(),
                kind: "resource.observed".into(),
                actor: Some("person/avery".into()),
                fields: BTreeMap::from([
                    ("kind".into(), Value::String("human.review".into())),
                    ("reason".into(), Value::String(format!("change {n}"))),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
    };
    observe(0);
    let before = store.index().unwrap();
    let seen = store
        .read_snapshot(|index| {
            assert_eq!(index, before);
            // Another thread commits in the middle of the read. The writer sees it; the read
            // does not.
            std::thread::scope(|scope| {
                scope.spawn(|| observe(1));
            });
            assert!(store.index().unwrap() > index);
            let claims = store.claims_for("resource/pinned", None)?.len();
            let nested = store.read_snapshot(|nested| {
                Ok((nested, store.claims_for("resource/pinned", None)?.len()))
            })?;
            assert_eq!(
                nested,
                (index, claims),
                "a nested read joins the outer snapshot"
            );
            Ok(claims)
        })
        .unwrap();
    assert_eq!(seen, 1);
    assert_eq!(store.claims_for("resource/pinned", None).unwrap().len(), 2);
    // Every pinned connection went back to the pool, even after a failed read.
    for _ in 0..32 {
        let _ = store.read_snapshot(|_| -> Result<()> { anyhow::bail!("a failed read") });
        store.read_snapshot(|_| Ok(())).unwrap();
    }
    assert_eq!(store.claims_for("resource/pinned", None).unwrap().len(), 2);
}

#[test]
fn a_read_opens_another_connection_rather_than_wait_for_one() {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(&directory.path().join("state.sqlite3"), "node").unwrap());
    let (ready, readied) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel::<()>();
    let holder = {
        let store = store.clone();
        std::thread::spawn(move || {
            store.hold_read_connections_for_test(|| {
                ready.send(()).unwrap();
                released.recv().unwrap();
            });
        })
    };
    readied.recv().unwrap();
    // Every connection the pool kept is busy, and more besides. A read and a pinned read
    // that fans out to worker threads still answer at once.
    let (done, finished) = std::sync::mpsc::channel();
    for _ in 0..8 {
        let (store, done) = (store.clone(), done.clone());
        std::thread::spawn(move || {
            let result = store.read_snapshot(|_| {
                std::thread::scope(|scope| {
                    scope
                        .spawn(|| store.claims_for("resource/busy", None))
                        .join()
                        .unwrap()
                })
            });
            done.send(result.is_ok()).unwrap();
        });
    }
    for _ in 0..8 {
        assert!(
            finished
                .recv_timeout(std::time::Duration::from_secs(5))
                .expect("a read waited for a busy connection")
        );
    }
    release.send(()).unwrap();
    holder.join().unwrap();
    // Idle retention does not bound concurrently open connections.
    assert!(
        store.readers.idle.lock().unwrap().len() <= max_idle_read_connections()
    );
}

#[test]
fn pinned_reads_that_fan_out_never_wait_on_each_other() {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(&directory.path().join("state.sqlite3"), "node").unwrap());
    // Every pinned read holds its connection while a worker thread, like the agent status
    // reduction's, needs another one from the pool.
    let (done, finished) = std::sync::mpsc::channel();
    let barrier = Arc::new(std::sync::Barrier::new(STATUS_WORKERS));
    for _ in 0..STATUS_WORKERS {
        let (store, done, barrier) = (store.clone(), done.clone(), barrier.clone());
        std::thread::spawn(move || {
            barrier.wait();
            let result = store.read_snapshot(|_| {
                std::thread::sleep(std::time::Duration::from_millis(50));
                std::thread::scope(|scope| {
                    scope
                        .spawn(|| {
                            store
                                .claims_for("resource/pinned", None)
                                .map(|claims| claims.len())
                        })
                        .join()
                        .unwrap()
                })
            });
            done.send(result.is_ok()).unwrap();
        });
    }
    for _ in 0..STATUS_WORKERS {
        assert!(
            finished
                .recv_timeout(std::time::Duration::from_secs(10))
                .expect("a pinned read waited forever for a connection")
        );
    }
}

#[test]
fn an_inbound_exchange_keeps_the_outbound_grant_refusal_until_a_dial_succeeds() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("amber.db");
    let fleet = "94cd11ba-c582-4558-9c84-c3bda922eb6d";
    let store = Store::open(&path, "amber").unwrap();
    store.bind_fleet(fleet).unwrap();
    store
        .record_peer_failure(
            "cobalt",
            "refused",
            "refused by that member's Fabric grants",
        )
        .unwrap();
    drop(store);
    let store = Store::open(&path, "amber").unwrap();
    let peer = Store::open_memory("cobalt").unwrap();
    peer.bind_fleet(fleet).unwrap();
    let exchange = peer
        .export_replication_exchange(fleet, &ReplicationInventory::default())
        .unwrap();
    store
        .receive_replication_exchange("cobalt", fleet, &exchange)
        .unwrap();
    let status = store
        .replication_status(true, Some(fleet), &["cobalt".into()])
        .unwrap();
    assert_eq!(status.peers[0].status, "refused");
    assert!(status.peers[0].last_success_at_unix_ms.is_some());
    assert!(status.peers[0].last_error.is_none());
    store
        .receive_replication_exchange_asking("cobalt", fleet, &exchange, true)
        .unwrap();
    let status = store
        .replication_status(true, Some(fleet), &["cobalt".into()])
        .unwrap();
    assert_eq!(status.peers[0].status, "up");
    assert!(status.peers[0].refusal_reason.is_none());
}

#[test]
fn recent_up_observation_prevents_a_transport_timeout_flap() {
    let store = Store::open_memory("source").unwrap();
    store
        .record_transport_observation("target", "up", None, None)
        .unwrap();
    assert_eq!(store.replication_peer_last_success("target").unwrap(), None);
    assert!(
        !store
            .record_peer_failure("target", "down", "request timed out")
            .unwrap()
    );
    assert_eq!(
        store
            .latest_claim("host/target", Some("transport.observed"))
            .unwrap()
            .unwrap()
            .body["fields"]["status"],
        "up"
    );

    let stale = Store::open_memory("source").unwrap();
    stale
        .record_transport_observation(
            "target",
            "up",
            None,
            Some(now_ms().saturating_sub(91_000)),
        )
        .unwrap();
    assert!(
        stale
            .record_peer_failure("target", "down", "request timed out")
            .unwrap()
    );

    // Once an exchange has recorded a success, that row is the evidence, since every
    // exchange updates it. When it is old, a timeout goes through even though the newest
    // observation, from this node, still looks fresh.
    store
        .connection
        .lock()
        .unwrap()
        .execute(
            "INSERT OR REPLACE INTO replication_peers(peer, status, last_success_at_unix_ms, updated_at_unix_ms)
                 VALUES ('target', 'up', ?1, ?2)",
            params![
                now_ms().saturating_sub(91_000).to_string(),
                now_ms().to_string()
            ],
        )
        .unwrap();
    assert!(
        store
            .record_peer_failure("target", "down", "request timed out")
            .unwrap()
    );
}

/// A peer that stops answering (#1020) shows its failure at once, and stops counting as up
/// once it has missed an exchange; a failure within one quiet interval of an exchange in
/// the other direction keeps it up. The next exchange clears the failure.
#[test]
fn a_failed_attempt_after_a_missed_exchange_ends_up() {
    let fleet = "94cd11ba-c582-4558-9c84-c3bda922eb6d";
    let store = Store::open_memory("amber").unwrap();
    store.bind_fleet(fleet).unwrap();
    let last_exchange = |ago: u128| {
        store
            .connection
            .lock()
            .unwrap()
            .execute(
                "INSERT OR REPLACE INTO replication_peers(peer, status, last_success_at_unix_ms, updated_at_unix_ms)
                     VALUES ('cobalt', 'up', ?1, ?1)",
                [now_ms().saturating_sub(ago).to_string()],
            )
            .unwrap();
    };
    let peer = || {
        store
            .replication_status(true, Some(fleet), &["cobalt".into()])
            .unwrap()
            .peers
            .remove(0)
    };

    last_exchange(10_000);
    store
        .record_peer_failure("cobalt", "down", "request timed out")
        .unwrap();
    let status = peer();
    assert_eq!(
        status.status, "up",
        "a one-way failure soon after an exchange"
    );
    assert_eq!(status.last_error.as_deref(), Some("request timed out"));
    assert!(status.last_failure_at_unix_ms.is_some());

    last_exchange(40_000);
    assert!(
        !store
            .record_peer_failure("cobalt", "down", "request timed out")
            .unwrap(),
        "the published transport status keeps its grace"
    );
    let status = peer();
    assert_eq!(status.status, "last-seen");
    assert_eq!(status.last_error.as_deref(), Some("request timed out"));
    assert!(!store.replication_peer_up("cobalt").unwrap().0);

    let other = Store::open_memory("cobalt").unwrap();
    other.bind_fleet(fleet).unwrap();
    let exchange = other
        .export_replication_exchange(fleet, &ReplicationInventory::default())
        .unwrap();
    store
        .receive_replication_exchange("cobalt", fleet, &exchange)
        .unwrap();
    let status = peer();
    assert_eq!(status.status, "up");
    assert!(status.last_error.is_none());
    assert!(status.last_failure_at_unix_ms.is_none());
    assert!(store.replication_peer_up("cobalt").unwrap().0);
}

/// A measurement the peer has not refreshed for a quiet interval says what both held then
/// (#1020): status marks it stale and counts the envelopes this node gained since.
#[test]
fn an_old_sync_measurement_is_stale_and_counts_what_was_added_since() {
    let fleet = "94cd11ba-c582-4558-9c84-c3bda922eb6d";
    let store = Store::open_memory("amber").unwrap();
    store.bind_fleet(fleet).unwrap();
    let other = Store::open_memory("cobalt").unwrap();
    other.bind_fleet(fleet).unwrap();
    let exchange = other
        .export_replication_exchange(fleet, &ReplicationInventory::default())
        .unwrap();
    store
        .receive_replication_exchange("cobalt", fleet, &exchange)
        .unwrap();
    let sync = |store: &Store| {
        store
            .replication_status(true, Some(fleet), &["cobalt".into()])
            .unwrap()
            .peers
            .remove(0)
            .sync
            .unwrap()
    };
    let fresh = sync(&store);
    assert!(!fresh.stale);
    assert_eq!(fresh.added_since_measured_envelopes, 0);

    for index in 0..20 {
        store
            .append_claim(&ClaimInput {
                subject: format!("custom/frozen-peer/{index}"),
                kind: "custom.frozen-peer.note".into(),
                actor: Some("person/tester".into()),
                fields: BTreeMap::new(),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
    }
    store
        .replication_sync
        .lock()
        .unwrap()
        .get_mut("cobalt")
        .unwrap()
        .measured
        .as_mut()
        .unwrap()
        .measured_at_unix_ms -= 40_000;
    let old = sync(&store);
    assert!(old.stale);
    assert_eq!(old.local_only_envelopes, fresh.local_only_envelopes);
    assert!(
        old.added_since_measured_envelopes >= 20,
        "{}",
        old.added_since_measured_envelopes
    );
}

/// A start repairs operation rows that drifted (#898). It rewrites only those rows: rebuilding
/// the whole table held the only writer for a minute and more on a populated store.
#[test]
fn an_operation_drift_repair_rewrites_only_the_drifted_rows() {
    let store = Store::open_memory("node").unwrap();
    for index in 0..200 {
        store
            .append_claim(&ClaimInput {
                subject: format!("custom/drift/{index}"),
                kind: "custom.drift.note".into(),
                actor: Some("person/tester".into()),
                fields: BTreeMap::new(),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some(format!("drift-{index}")),
            })
            .unwrap();
    }
    let (rows, ids) = {
        let connection = store.connection.lock().unwrap();
        let ids = connection
            .prepare("SELECT id FROM operations ORDER BY id LIMIT 2")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        let rows: i64 = connection
            .query_row("SELECT COUNT(*) FROM operations", [], |row| row.get(0))
            .unwrap();
        (rows, ids)
    };
    assert!(rows >= 200, "{rows} operations");
    assert!(store.operation_projection_drift().unwrap().is_empty());
    // One row changed, one row lost, and one row that no claim backs.
    {
        let connection = store.connection.lock().unwrap();
        connection
            .execute(
                "UPDATE operations SET state='conflict' WHERE id=?1",
                [&ids[0]],
            )
            .unwrap();
        connection
            .execute("DELETE FROM operations WHERE id=?1", [&ids[1]])
            .unwrap();
        connection
            .execute(
                "INSERT INTO operations(id, request_digest, canonical_claim_id, state)
                     SELECT 'op/invented-stray', 'digest', canonical_claim_id, 'active'
                     FROM operations WHERE id=?1",
                [&ids[0]],
            )
            .unwrap();
    }
    assert_eq!(store.operation_projection_drift().unwrap().len(), 3);
    // Rows written by the repair, besides the ones its triggers write.
    let operation_rows = || {
        store
            .connection
            .lock()
            .unwrap()
            .query_row("SELECT total_changes()", [], |row| row.get::<_, i64>(0))
            .unwrap()
    };
    let before = operation_rows();
    assert!(store.repair_operation_projection_drift().unwrap());
    assert!(store.operation_projection_drift().unwrap().is_empty());
    let written = operation_rows() - before;
    assert!(
        written <= 3 * 4,
        "the repair wrote {written} rows for 3 drifted ones"
    );
    assert!(!store.repair_operation_projection_drift().unwrap());
}

/// The drift audit reads every claim an operation row can come from. Walking the whole
/// claims table evaluated json_extract on every claim in the store, seconds of work and
/// hundreds of megabytes of reads on a populated member, while the partial operation index
/// already names exactly the claims the audit keeps.
#[test]
fn the_operation_audit_walks_the_partial_operation_index() {
    let store = Store::open_memory("node").unwrap();
    let connection = store.readers.get();
    let plan = connection
        .prepare(&format!(
            "EXPLAIN QUERY PLAN {}",
            smallclaims::store::EXPECTED_OPERATIONS_QUERY
        ))
        .unwrap()
        .query_map([], |row| row.get::<_, String>(3))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
        .join("; ");
    assert!(
        plan.contains("USING INDEX claims_operation_index")
            && !plan.contains("USING INDEX sqlite_autoindex_claims_1"),
        "{plan}"
    );
}

#[test]
fn persistent_store_uses_bounded_sqlite_page_caches() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(&directory.path().join("state.sqlite3"), "node").unwrap();
    let writer_cache_kib: i64 = store
        .connection
        .lock()
        .unwrap()
        .query_row("PRAGMA cache_size", [], |row| row.get(0))
        .unwrap();
    let reader_cache_kib: i64 = store
        .readers
        .get()
        .query_row("PRAGMA cache_size", [], |row| row.get(0))
        .unwrap();
    assert_eq!(writer_cache_kib, -32768);
    assert_eq!(reader_cache_kib, -(smallclaims::sqlite::read_cache_kib() as i64));
}

#[test]
fn subscription_retry_queries_use_history_indexes() {
    let store = Store::open_memory("node").unwrap();
    let subject = "subscription/reviews";
    let first = now_ms() as u64 + 10_000;
    let second = first + 20_000;
    for (request, deadline) in [
        ("request-a", first),
        ("request-b", first),
        ("request-a", second),
    ] {
        store
            .append_claim(&ClaimInput {
                subject: subject.into(),
                kind: "subscription.mission-deferred".into(),
                actor: None,
                fields: BTreeMap::from([
                    ("request".into(), Value::String(request.into())),
                    ("not_before_unix_ms".into(), Value::from(deadline)),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
    }
    assert_eq!(
        store
            .subscription_mission_deferral(subject, "request-a")
            .unwrap(),
        Some((u128::from(second), 2)),
    );
    assert_eq!(
        store
            .subscription_mission_deferral(subject, "request-b")
            .unwrap(),
        Some((u128::from(first), 1)),
    );
    assert_eq!(
        store
            .subscription_mission_deferral(subject, "missing")
            .unwrap(),
        None,
    );
    let before = store.index().unwrap();
    let third = second + 30_000;
    store
        .record_subscription_mission_deferral(subject, "request-a", third.into(), 3)
        .unwrap();
    assert_eq!(store.index().unwrap(), before);
    assert_eq!(
        store
            .subscription_mission_deferral(subject, "request-a")
            .unwrap(),
        Some((third.into(), 3)),
    );
    assert_eq!(
        store.next_subscription_mission_retry_deadline().unwrap(),
        Some(first.into()),
    );
    let connection = store.readers.get();
    let request_plan: String = connection
        .query_row(
            "EXPLAIN QUERY PLAN SELECT COUNT(*) FROM claims
                 WHERE subject=?1 AND kind='subscription.mission-deferred'
                   AND json_extract(body, '$.fields.request')=?2",
            params![subject, "request-a"],
            |row| row.get(3),
        )
        .unwrap();
    assert!(
        request_plan.contains("claims_subscription_deferred_request_index"),
        "{request_plan}"
    );
    let deadline_plan: String = connection
        .query_row(
            "EXPLAIN QUERY PLAN SELECT MIN(CAST(json_extract(body, '$.fields.not_before_unix_ms') AS INTEGER))
                 FROM claims INDEXED BY claims_subscription_deferred_deadline_index
                 WHERE kind='subscription.mission-deferred'
                   AND CAST(json_extract(body, '$.fields.not_before_unix_ms') AS INTEGER)>?1",
            [now_ms() as i64],
            |row| row.get(3),
        )
        .unwrap();
    assert!(
        deadline_plan.contains("claims_subscription_deferred_deadline_index"),
        "{deadline_plan}"
    );
}

#[test]
fn local_subscription_deferral_survives_a_daemon_restart() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("state.sqlite3");
    let deadline = now_ms() + 30_000;
    {
        let store = Store::open(&path, "node").unwrap();
        let before = store.index().unwrap();
        store
            .record_subscription_mission_deferral(
                "subscription/reviews",
                "request-a",
                deadline,
                7,
            )
            .unwrap();
        assert_eq!(store.index().unwrap(), before);
    }
    let reopened = Store::open(&path, "node").unwrap();
    assert_eq!(
        reopened
            .subscription_mission_deferral("subscription/reviews", "request-a")
            .unwrap(),
        Some((deadline, 7)),
    );
    assert_eq!(
        reopened.next_subscription_mission_retry_deadline().unwrap(),
        Some(deadline),
    );
}

#[test]
fn replication_projection_retries_missing_and_stale_health_only() {
    let store = Store::open_memory("node").unwrap();
    assert!(store.replication_projection_needs_recovery().unwrap());
    assert!(store.project_replication_backlog().unwrap());
    assert!(!store.replication_projection_needs_recovery().unwrap());
    store
        .connection
        .lock()
        .unwrap()
        .execute(
            "UPDATE projection_health SET status='stale' WHERE aggregate='graph'",
            [],
        )
        .unwrap();
    assert!(store.replication_projection_needs_recovery().unwrap());
}

#[test]
fn pending_request_queries_return_only_unstarted_history() {
    let store = Store::open_memory("node").unwrap();
    let mut connection = store.connection.lock().unwrap();
    let transaction = connection.transaction().unwrap();
    for sequence in 0..100 {
        for (subject, requested, started) in [
            (
                "subscription/test",
                "subscription.mission-requested",
                "subscription.mission-started",
            ),
            (
                "schedule/test",
                "schedule.work-requested",
                "schedule.work-started",
            ),
        ] {
            let body = if requested == "subscription.mission-requested" {
                json!({"fields": {
                    "mission": "mission/test", "mission_revision": "revision",
                    "resource": "resource/test", "resource_input": "source",
                    "workspace": "/tmp/test", "discovery": format!("discovery-{sequence}")
                }})
            } else {
                json!({"fields": {
                    "revision": "revision", "occurrence": sequence,
                    "mission": "mission/test", "mission_revision": "revision",
                    "workspace": "/tmp/test", "inputs": {}
                }})
            };
            let request = append_claim_tx(
                &transaction,
                &store.origin,
                subject,
                requested,
                None,
                &body,
                &[],
                None,
            )
            .unwrap();
            append_claim_tx(
                &transaction,
                &store.origin,
                subject,
                started,
                None,
                &json!({"fields": {"request": request.id, "mission_run": "mission-run/test"}}),
                &[],
                None,
            )
            .unwrap();
        }
    }
    let subscription = append_claim_tx(
        &transaction,
        &store.origin,
        "subscription/test",
        "subscription.mission-requested",
        None,
        &json!({"fields": {
            "mission": "mission/test", "mission_revision": "revision",
            "resource": "resource/test", "resource_input": "source",
            "workspace": "/tmp/test", "discovery": "discovery-pending"
        }}),
        &[],
        None,
    )
    .unwrap();
    let schedule = append_claim_tx(
        &transaction,
        &store.origin,
        "schedule/test",
        "schedule.work-requested",
        None,
        &json!({"fields": {
            "revision": "revision", "occurrence": 101,
            "mission": "mission/test", "mission_revision": "revision",
            "workspace": "/tmp/test", "inputs": {}
        }}),
        &[],
        None,
    )
    .unwrap();
    transaction.commit().unwrap();
    drop(connection);
    let pending = store
        .pending_subscription_mission_requests("subscription/test")
        .unwrap();
    assert_eq!(
        pending.iter().map(|claim| &claim.id).collect::<Vec<_>>(),
        vec![&subscription.id]
    );
    let pending = store
        .pending_schedule_work_requests("schedule/test")
        .unwrap();
    assert_eq!(
        pending.iter().map(|claim| &claim.id).collect::<Vec<_>>(),
        vec![&schedule.id]
    );
}

/// A start rebuilds the tables derived from claims once, then keeps them: every write keeps
/// them current, and rebuilding them read every claim before the API could answer.
#[test]
fn a_start_keeps_the_derived_tables_it_built_before() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("claims.sqlite3");
    drop(Store::open(&path, "node").unwrap());
    {
        let connection = Connection::open(&path).unwrap();
        projection_digest::register(&connection).unwrap();
        connection
            .execute_batch("PRAGMA foreign_keys = OFF")
            .unwrap();
        connection
            .execute(
                "INSERT INTO operations(id, request_digest, canonical_claim_id, state)
                     VALUES ('op/kept', 'digest', 'claim/kept', 'active')",
                [],
            )
            .unwrap();
    }
    let reopened = Store::open(&path, "node").unwrap();
    let kept: bool = reopened
        .connection
        .lock()
        .unwrap()
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM operations WHERE id='op/kept')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        kept,
        "a start with current derived tables must not rebuild them"
    );
    drop(reopened);
    {
        let connection = Connection::open(&path).unwrap();
        connection
            .execute("DELETE FROM meta WHERE key='derived_tables_version'", [])
            .unwrap();
    }
    let rebuilt = Store::open(&path, "node").unwrap();
    let kept: bool = rebuilt
        .connection
        .lock()
        .unwrap()
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM operations WHERE id='op/kept')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(!kept, "a store no build with this rule opened is rebuilt");
}

#[test]
fn simple_replication_advances_events_without_full_replay() {
    let store = Store::open_memory("node").unwrap();
    assert!(store.project_replication_backlog().unwrap());
    let claim = {
        let mut connection = store.connection.lock().unwrap();
        let transaction = connection.transaction().unwrap();
        let claim = append_claim_tx(
            &transaction,
            &store.origin,
            "agent/node.test",
            "harness.observed",
            Some("agent/node.test"),
            &json!({"fields": {"state": "ready"}}),
            &[],
            None,
        )
        .unwrap();
        assert!(try_project_all_simple_replication_tx(&transaction, &store.origin).unwrap());
        transaction.commit().unwrap();
        claim
    };
    let connection = store.connection.lock().unwrap();
    let event_count: u64 = connection
        .query_row(
            "SELECT COUNT(*) FROM events WHERE store_index=?1",
            [claim.store_index],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(event_count, 1);
}

#[test]
fn resource_observation_projection_does_not_rebuild_operation_history() {
    let store = Store::open_memory("node").unwrap();
    assert!(store.project_replication_backlog().unwrap());
    {
        let mut connection = store.connection.lock().unwrap();
        let transaction = connection.transaction().unwrap();
        append_claim_tx(
            &transaction,
            &store.origin,
            "agent/node.test",
            "harness.observed",
            Some("agent/node.test"),
            &json!({"fields": {"state": "ready"}, "_operation": {
                "id": "op/existing", "request_digest": "digest-one"
            }}),
            &[],
            None,
        )
        .unwrap();
        transaction.commit().unwrap();
    }
    assert!(store.project_replication_backlog().unwrap());
    {
        let mut connection = store.connection.lock().unwrap();
        let transaction = connection.transaction().unwrap();
        append_claim_tx(
            &transaction,
            &store.origin,
            "resource/repo",
            "resource.observed",
            None,
            &json!({"fields": {"kind": "vcs.repository", "state": "active"}}),
            &[],
            None,
        )
        .unwrap();
        transaction.commit().unwrap();
    }
    let before = store.connection.lock().unwrap().total_changes();
    assert!(store.project_replication_backlog().unwrap());
    let connection = store.connection.lock().unwrap();
    assert!(connection.total_changes() - before <= 2);
    assert!(operation_tx(&connection, "op/existing").unwrap().is_some());
}

/// A full replay projects each claim in its own savepoint and quarantines one it cannot
/// project. Once the transaction has changed the schema, as a checkpoint proof's does with
/// its temporary tables, SQLite aborts every open read when a savepoint rolls back. The
/// replay must not be reading then, or one malformed claim fails every proof on that node.
#[test]
fn a_replay_quarantines_claims_it_cannot_project_and_projects_the_rest() {
    const FLEET: &str = "5e3c1a9b-2d4f-4b6e-8a7c-0f1e2d3c4b5a";
    let source = Store::open_memory("source").unwrap();
    for n in 0..3 {
        source
            .append_claim(&ClaimInput {
                subject: format!("mission-run/broken-{n}"),
                kind: "mission-run.created".into(),
                actor: None,
                fields: BTreeMap::new(),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some(format!("broken-{n}")),
            })
            .unwrap();
    }
    let later = source
        .append_claim(&ClaimInput {
            subject: "daemon/source".into(),
            kind: "daemon.started".into(),
            actor: None,
            fields: BTreeMap::from([("status".into(), json!("running"))]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some("started".into()),
        })
        .unwrap();
    source.bind_fleet(FLEET).unwrap();
    let target = Store::open_memory("target").unwrap();
    target.bind_fleet(FLEET).unwrap();
    let exchange = source
        .export_replication_exchange(FLEET, &target.replication_inventory().unwrap())
        .unwrap();
    target
        .receive_replication_exchange("source", FLEET, &exchange)
        .unwrap();
    target.validate_replication_backlog().unwrap();
    target.project_replication_backlog().unwrap();
    let connection = target.readers.get();
    let quarantined: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM projection_health
                 WHERE aggregate LIKE 'projection:runs:%' AND status='stale'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(quarantined, 3);
    let healthy: String = connection
        .query_row(
            "SELECT status FROM projection_health WHERE aggregate='graph'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(healthy, "healthy");
    assert!(
        target
            .latest_claim("daemon/source", Some("daemon.started"))
            .unwrap()
            .is_some_and(|claim| claim.id == later.id)
    );
    drop(connection);
    // The proof replays the same claims after creating its temporary tables.
    let scratch = tempfile::tempdir().unwrap();
    let (_, _, proof) = target
        .plan_checkpoint_through(now_ms() + 1_000, None, scratch.path())
        .unwrap();
    assert!(proof.passed, "{proof:?}");
}

/// Writes that arrive together share one transaction and one commit, and each caller hears
/// back only once that commit is on disk.
#[test]
fn concurrent_claims_share_the_writer_s_commits() {
    let directory = tempfile::tempdir().unwrap();
    let store =
        Arc::new(Store::open(&directory.path().join("claims.sqlite3"), "node").unwrap());
    let start = Arc::new(std::sync::Barrier::new(32));
    let writers = (0..32)
        .map(|index| {
            let (store, start) = (store.clone(), start.clone());
            std::thread::spawn(move || {
                start.wait();
                store
                    .append_claim(&ClaimInput {
                        subject: format!("agent/node.writer-{index}"),
                        kind: "harness.usage".into(),
                        actor: Some(format!("agent/node.writer-{index}")),
                        fields: serde_json::from_value(json!({
                            "driver": "codex", "incarnation_id": "current",
                            "semantics": "response", "total_tokens": index
                        }))
                        .unwrap(),
                        evidence: Vec::new(),
                        expected_subject: None,
                        idempotency_key: Some(format!("usage-{index}")),
                    })
                    .unwrap()
            })
        })
        .collect::<Vec<_>>();
    let claims = writers
        .into_iter()
        .map(|writer| writer.join().unwrap())
        .collect::<Vec<_>>();
    let (batches, writes) = (
        store.connection.batches.0.load(Ordering::Relaxed),
        store.connection.batches.1.load(Ordering::Relaxed),
    );
    assert_eq!(writes, 32, "{batches} batches");
    assert!(batches < 32, "32 concurrent claims took {batches} commits");
    // Each claim is committed when its writer hears back.
    for claim in claims {
        assert!(store.claim_by_id(&claim.id).unwrap().is_some());
    }
}

/// A batched write that fails or panics undoes only its own savepoint: the writes that share
/// its transaction still commit, the failed caller gets its error and the panic reaches the
/// caller that panicked.
#[test]
fn a_failed_batched_write_undoes_only_itself() {
    let store = Arc::new(Store::open_memory("node").unwrap());
    let insert = |transaction: &Transaction<'_>, key: &str| {
        transaction
            .execute("INSERT INTO meta(key, value) VALUES (?1, 'written')", [key])
            .map(drop)
    };
    // Hold the writer so the three writes queue behind it and share the next batch.
    let held = store.connection.write();
    let writers = ["succeeds", "fails", "panics"].map(|outcome| {
        let store = store.clone();
        std::thread::spawn(move || {
            store
                .connection
                .batched(|transaction| -> Result<(), String> {
                    insert(transaction, &format!("batch-test-{outcome}"))
                        .map_err(|error| error.to_string())?;
                    match outcome {
                        "fails" => Err("refused".into()),
                        "panics" => panic!("a batched write panicked"),
                        _ => Ok(()),
                    }
                })
                .unwrap()
        })
    });
    std::thread::sleep(std::time::Duration::from_millis(100));
    drop(held);
    let [succeeds, fails, panics] = writers.map(|writer| writer.join());
    assert_eq!(succeeds.unwrap(), Ok(()));
    assert_eq!(fails.unwrap(), Err("refused".into()));
    assert!(panics.is_err());
    let reader = store.readers.get();
    let written = |key: &str| {
        reader
            .query_row("SELECT 1 FROM meta WHERE key=?1", [key], |_| Ok(()))
            .optional()
            .unwrap()
            .is_some()
    };
    assert!(written("batch-test-succeeds"));
    assert!(!written("batch-test-fails"));
    assert!(!written("batch-test-panics"));
    // The writer keeps working after both.
    store
        .connection
        .batched(|transaction| insert(transaction, "batch-test-after"))
        .unwrap()
        .unwrap();
    assert!(written("batch-test-after"));
}

/// A batch that cannot begin, because another process holds SQLite's write lock past the busy
/// timeout, answers each of its writes with that error instead of losing them.
#[test]
fn a_batch_that_cannot_begin_answers_its_writes_with_the_error() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("claims.sqlite3");
    let store = Store::open(&path, "node").unwrap();
    store
        .connection
        .write()
        .execute_batch("PRAGMA busy_timeout = 50")
        .unwrap();
    let other = Connection::open(&path).unwrap();
    other.execute_batch("BEGIN IMMEDIATE").unwrap();
    let refused = store.connection.batched(|transaction| {
        transaction.execute(
            "INSERT INTO meta(key, value) VALUES ('refused', 'written')",
            [],
        )
    });
    let error = refused.expect_err("the batch could not begin");
    assert!(error.contains("locked"), "{error}");
    other.execute_batch("ROLLBACK").unwrap();
    store
        .connection
        .batched(|transaction| {
            transaction.execute(
                "INSERT INTO meta(key, value) VALUES ('after', 'written')",
                [],
            )
        })
        .unwrap()
        .unwrap();
}

/// A panic while the writer is held leaves no half-written transaction behind, so it must not
/// turn every later write into a panic while the daemon keeps running.
#[test]
fn a_panic_while_the_writer_is_held_does_not_disable_the_store() {
    let store = Arc::new(Store::open_memory("node").unwrap());
    let panicking = store.clone();
    let panicked = std::thread::spawn(move || {
        let _writer = panicking.connection.write();
        panic!("a panic while the writer is held");
    })
    .join();
    assert!(panicked.is_err());
    store
        .append_claim(&ClaimInput {
            subject: "daemon/node".into(),
            kind: "daemon.diagnostic".into(),
            actor: None,
            fields: BTreeMap::from([
                ("severity".into(), Value::String("error".into())),
                ("code".into(), Value::String("after-a-panic".into())),
                (
                    "reason".into(),
                    Value::String("the store still writes".into()),
                ),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
}

/// A claim this build cannot project, such as a desired body from another build, is
/// quarantined and named. Every other claim still reaches the graph.
#[test]
fn one_claim_that_cannot_be_projected_is_quarantined_while_the_rest_project() {
    let store = Store::open_memory("node").unwrap();
    let source = "version 2\nagent \"bad\" { workspace \"/tmp\"; command \"true\" }\nagent \"good\" { workspace \"/tmp\"; command \"true\" }\n";
    let intent = parse_intent(source, "node").unwrap();
    let preview = store
        .mission(
            &intent,
            IntentInput {
                kdl: source.into(),
                source_name: None,
            },
        )
        .unwrap();
    store
        .apply(&intent, &preview.subject_tokens, "quarantine")
        .unwrap();
    let bad_claim = {
        let connection = store.connection.lock().unwrap();
        let bad_claim: String = connection
            .query_row(
                "SELECT claim_id FROM desired WHERE subject='agent/node.bad'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        connection
            .execute(
                "UPDATE claims SET body='{\"from\":\"another build\"}' WHERE id=?1",
                [&bad_claim],
            )
            .unwrap();
        connection.execute("DELETE FROM desired", []).unwrap();
        connection
            .execute(
                "UPDATE projection_health SET status='stale' WHERE aggregate='graph'",
                [],
            )
            .unwrap();
        bad_claim
    };
    assert!(store.project_replication_backlog().unwrap());
    let desired = store.desired_subjects().unwrap();
    assert!(
        desired
            .iter()
            .any(|subject| subject.subject == "agent/node.good")
    );
    assert!(
        !desired
            .iter()
            .any(|subject| subject.subject == "agent/node.bad")
    );
    let status = store.replication_status(false, None, &[]).unwrap();
    assert_eq!(status.unhealthy_projections, 1);
    assert_eq!(
        status.unhealthy[0].aggregate,
        format!("projection:base:{bad_claim}")
    );
    assert!(
        status.unhealthy[0]
            .error_message
            .as_deref()
            .is_some_and(|message| message.contains("agent/node.bad"))
    );
    // A later full replay decides the claim again instead of keeping a stale quarantine.
    {
        let connection = store.connection.lock().unwrap();
        connection
            .execute("DELETE FROM claims WHERE id=?1", [&bad_claim])
            .unwrap();
        connection
            .execute(
                "UPDATE projection_health SET status='stale' WHERE aggregate='graph'",
                [],
            )
            .unwrap();
    }
    assert!(store.project_replication_backlog().unwrap());
    assert_eq!(
        store
            .replication_status(false, None, &[])
            .unwrap()
            .unhealthy_projections,
        0
    );
}

/// One repair that cannot be applied never stops the others, replication, or startup.
#[test]
fn one_repair_that_cannot_be_applied_is_named_and_the_rest_apply() {
    let store = Store::open_memory("node").unwrap();
    let source = "version 2\nagent \"one\" { workspace \"/tmp\"; command \"true\" }\nagent \"two\" { workspace \"/tmp\"; command \"true\" }\n";
    let intent = parse_intent(source, "node").unwrap();
    let preview = store
        .mission(
            &intent,
            IntentInput {
                kdl: source.into(),
                source_name: None,
            },
        )
        .unwrap();
    store
        .apply(&intent, &preview.subject_tokens, "repairs")
        .unwrap();
    let broken = {
        let mut connection = store.connection.lock().unwrap();
        let transaction = connection.transaction().unwrap();
        let mut repairs = Vec::new();
        for (position, agent) in ["one", "two"].into_iter().enumerate() {
            let subject = format!("agent/node.{agent}");
            let repaired: String = transaction
                .query_row(
                    "SELECT claim_id FROM desired WHERE subject=?1",
                    [&subject],
                    |row| row.get(0),
                )
                .unwrap();
            let body: String = transaction
                .query_row("SELECT body FROM claims WHERE id=?1", [&repaired], |row| {
                    row.get(0)
                })
                .unwrap();
            let replacement = append_claim_tx(
                &transaction,
                &store.origin,
                &subject,
                "intent.desired",
                None,
                &serde_json::from_str::<Value>(&body).unwrap(),
                &[],
                None,
            )
            .unwrap()
            .id;
            transaction
                .execute(
                    "INSERT INTO replica_records(record_ref, writer, sequence, envelope_hash,
                            position, raw, state, claim_id, updated_at_unix_ms)
                         VALUES (?1, 'peer', 1, 'envelope', ?2, x'00', 'valid', ?3, '0')",
                    params![format!("record/{agent}"), position as i64, repaired],
                )
                .unwrap();
            repairs.push((
                replacement.clone(),
                append_claim_tx(
                    &transaction,
                    &store.origin,
                    &format!("repair/{agent}"),
                    "record.repaired",
                    None,
                    &json!({"fields": {
                        "record": format!("record/{agent}"),
                        "replacement": replacement,
                        "reason": "a test repair"
                    }}),
                    &[],
                    None,
                )
                .unwrap()
                .id,
            ));
        }
        // The first replacement names a desired body that this build cannot read.
        transaction
            .execute(
                "UPDATE claims SET body='{\"from\":\"another build\"}' WHERE id=?1",
                [&repairs[0].0],
            )
            .unwrap();
        transaction.commit().unwrap();
        repairs[0].1.clone()
    };
    store.apply_replication_repairs().unwrap();
    let status = store.replication_status(false, None, &[]).unwrap();
    assert_eq!(
        status
            .unhealthy
            .iter()
            .map(|projection| projection.aggregate.clone())
            .collect::<Vec<_>>(),
        [format!("repair:{broken}")]
    );
    assert_eq!(
        status.repaired_records, 1,
        "the second repair still applied"
    );
}

#[test]
fn structural_replication_advances_from_healthy_frontier() {
    let store = Store::open_memory("node").unwrap();
    assert!(store.project_replication_backlog().unwrap());
    {
        let mut connection = store.connection.lock().unwrap();
        let transaction = connection.transaction().unwrap();
        for id in 0..20 {
            append_claim_tx(
                &transaction,
                &store.origin,
                "agent/node.test",
                "harness.observed",
                Some("agent/node.test"),
                &json!({"fields": {"state": "ready"}, "_operation": {
                    "id": format!("op/structural-test/{id}"),
                    "request_digest": format!("digest-{id}")
                }}),
                &[],
                None,
            )
            .unwrap();
        }
        transaction.commit().unwrap();
    }
    assert!(store.project_replication_backlog().unwrap());
    store
        .connection
        .lock()
        .unwrap()
        .execute(
            "UPDATE claims SET accepted_at_unix_ms='1' WHERE kind='harness.observed'",
            [],
        )
        .unwrap();
    {
        let mut connection = store.connection.lock().unwrap();
        let transaction = connection.transaction().unwrap();
        transaction
            .execute(
                "INSERT INTO blobs(hash, bytes, size) VALUES ('sha256:abc', x'01', 1)",
                [],
            )
            .unwrap();
        append_claim_tx(
            &transaction,
            &store.origin,
            "doc/test/incremental",
            "doc.bound",
            Some("agent/node.test"),
            &json!({"name": "doc/test/incremental", "hash": "sha256:abc"}),
            &[],
            None,
        )
        .unwrap();
        transaction.commit().unwrap();
    }
    {
        let mut connection = store.connection.lock().unwrap();
        let transaction = connection.transaction().unwrap();
        assert!(try_project_all_simple_replication_tx(&transaction, &store.origin).unwrap());
        transaction.rollback().unwrap();
    }
    let before = store.connection.lock().unwrap().total_changes();
    assert!(store.project_replication_backlog().unwrap());
    let connection = store.connection.lock().unwrap();
    let changes = connection.total_changes() - before;
    assert!(
        changes < 10,
        "incremental projection changed {changes} rows"
    );
    let hash: String = connection
        .query_row(
            "SELECT hash FROM documents WHERE name='doc/test/incremental'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(hash, "sha256:abc");
}

#[test]
fn one_divergent_envelope_exchanges_only_its_inventory_range() {
    let observe = |store: &Store, subject: &str, number: usize| {
        store
            .append_claim(&ClaimInput {
                subject: subject.into(),
                kind: "transport.observed".into(),
                actor: None,
                fields: BTreeMap::from([
                    ("status".into(), Value::String("up".into())),
                    ("reason".into(), Value::String(format!("sample {number}"))),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
    };
    let remote = Store::open_memory("remote").unwrap();
    for number in 0..1_100 {
        observe(&remote, "host/remote", number);
    }
    let local = Store::open_memory("local").unwrap();
    for after in [0, 512, 1_024] {
        local
            .import_replication("remote", &remote.export_replication(after).unwrap())
            .unwrap();
    }
    observe(&remote, "host/remote", 1_100);

    // The local peer's summary starts the exchange. The remote lists one range, not the
    // 1,100 identities both peers already share.
    let summary = local.export_replication_summary(TEST_FLEET).unwrap();
    let response = remote
        .export_replication_exchange(TEST_FLEET, &summary.inventory)
        .unwrap();
    assert!(!response.inventory.envelopes.is_empty());
    assert!(response.inventory.envelopes.len() <= REPLICATION_BUCKET_WIDTH as usize);
    assert!(serde_json::to_vec(&response).unwrap().len() < 65_536);
    local
        .receive_replication_exchange("remote", TEST_FLEET, &response)
        .unwrap();
    let push = local
        .export_replication_exchange(TEST_FLEET, &response.inventory)
        .unwrap();
    assert!(push.inventory.envelopes.len() <= REPLICATION_BUCKET_WIDTH as usize);
    remote
        .receive_replication_exchange("local", TEST_FLEET, &push)
        .unwrap();
    let pull = remote
        .export_replication_exchange(TEST_FLEET, &push.inventory)
        .unwrap();
    assert_eq!(pull.envelopes.len(), 1);
    local
        .receive_replication_exchange("remote", TEST_FLEET, &pull)
        .unwrap();
    assert_eq!(
        local
            .export_replication_summary(TEST_FLEET)
            .unwrap()
            .inventory
            .digest,
        remote
            .export_replication_summary(TEST_FLEET)
            .unwrap()
            .inventory
            .digest
    );

    // An explicit full-inventory request still gets every identity.
    let explicit = local
        .export_replication_exchange(TEST_FLEET, &ReplicationInventory::default())
        .unwrap();
    assert!(explicit.inventory.envelopes.len() > 1_100);
}

#[test]
fn quiet_claim_detection_requires_only_usage_or_renewal_since_cursor() {
    let store = Store::open_memory("node").unwrap();
    let before = store.index().unwrap();
    assert!(!store.claims_since_only_quiet_notifications(before).unwrap());
    let append = |kind| {
        let mut connection = store.connection.lock().unwrap();
        let transaction = connection.transaction().unwrap();
        let fields = if kind == "harness.usage" {
            json!({"driver": "codex", "incarnation_id": "current", "semantics": "response", "total_tokens": 1})
        } else if kind == "harness.observed" {
            json!({"driver": "codex", "incarnation_id": "current", "state": "ready"})
        } else {
            json!({
                "status": "claimed", "attempt": 1, "readiness_epoch": 1,
                "claimant": "agent/node.test", "claim_incarnation": "current",
                "claim_expires_at_unix_ms": now_ms().saturating_add(60_000),
                "worker_reported": false
            })
        };
        append_claim_tx(
            &transaction,
            &store.origin,
            if kind == "work.renewed" {
                "step-run/test/work"
            } else {
                "agent/node.test"
            },
            kind,
            Some("agent/node.test"),
            &json!({"fields": fields}),
            &[],
            None,
        )
        .unwrap();
        transaction.commit().unwrap();
    };
    append("harness.usage");
    append("work.renewed");
    assert!(store.claims_since_only_quiet_notifications(before).unwrap());
    let after_quiet = store.index().unwrap();
    append("harness.observed");
    assert!(!store.claims_since_only_quiet_notifications(before).unwrap());
    assert!(
        !store
            .claims_since_only_quiet_notifications(after_quiet)
            .unwrap()
    );
}

#[test]
fn only_work_wake_messages_count_as_reconciler_changes() {
    let store = Store::open_memory("node").unwrap();
    let append = |subject: &str, kind: &str, fields: Value| {
        let mut connection = store.connection.lock().unwrap();
        let transaction = connection.transaction().unwrap();
        append_claim_tx(
            &transaction,
            &store.origin,
            subject,
            kind,
            Some("agent/node.test"),
            &json!({"fields": fields}),
            &[],
            None,
        )
        .unwrap();
        transaction.commit().unwrap();
    };
    let sent = |tags: Value| {
        json!({"from": "person/example", "to": "agent/node.test", "content": "A note.",
               "status": "sent", "tags": tags})
    };
    // A conversation message and its lifecycle never change what the reconciler decides.
    let before = store.index().unwrap();
    append("message/talk", "message.sent", sent(json!(["chat"])));
    for status in ["delivered", "read", "closed"] {
        append(
            "message/talk",
            &format!("message.{status}"),
            json!({"status": status}),
        );
    }
    assert!(store.claims_since_only_quiet_notifications(before).unwrap());
    // A work wake does, and so does the lifecycle of a message this store has not seen sent.
    let before = store.index().unwrap();
    append(
        "message/wake",
        "message.sent",
        sent(json!(["st3-work:step-run/test/work"])),
    );
    assert!(!store.claims_since_only_quiet_notifications(before).unwrap());
    let before = store.index().unwrap();
    append("message/wake", "message.read", json!({"status": "read"}));
    assert!(!store.claims_since_only_quiet_notifications(before).unwrap());
    let before = store.index().unwrap();
    append("message/unseen", "message.read", json!({"status": "read"}));
    assert!(!store.claims_since_only_quiet_notifications(before).unwrap());
}

#[test]
fn replay_progress_reports_work_without_advancing_an_uncommitted_frontier() {
    let store = Store::open_memory("node").unwrap();
    store.project_replication_backlog().unwrap();
    {
        let mut connection = store.connection.lock().unwrap();
        let transaction = connection.transaction().unwrap();
        for number in 0..2501 {
            append_claim_record_tx(
                &transaction,
                "node",
                "agent/node.test",
                "harness.observed",
                None,
                &json!({"fields":{"state":"ready", "sequence":number}}),
                &[],
                None,
            )
            .unwrap();
        }
        transaction
            .execute(
                "UPDATE projection_health SET status='stale' WHERE aggregate='graph'",
                [],
            )
            .unwrap();
        transaction.commit().unwrap();
    }
    let target = store.index().unwrap();
    let mut events = Vec::new();
    assert!(
        store
            .project_replication_backlog_with_progress("test", |event| events.push(event))
            .unwrap()
    );
    let claims = events
        .iter()
        .filter(|event| event.phase == "full-replay/base-claims")
        .collect::<Vec<_>>();
    assert_eq!(
        claims
            .iter()
            .map(|event| event.processed.unwrap())
            .collect::<Vec<_>>(),
        [0]
    );
    assert!(claims.iter().all(|event| event.frontier == 0
        && event.target == target
        && event.total == Some(0)));
    let committed = events.last().unwrap();
    assert_eq!(committed.phase, "projection-committed");
    assert_eq!(committed.frontier, target);
    let digest = store
        .replication_status(true, None, &[])
        .unwrap()
        .graph_digest;
    store.replay_replication_graph().unwrap();
    assert_eq!(
        store
            .replication_status(true, None, &[])
            .unwrap()
            .graph_digest,
        digest
    );
}

#[test]
fn base_replay_batches_selected_bodies_and_preserves_event_and_quarantine_answers() {
    let store = Store::open_memory("node").unwrap();
    let mut connection = store.connection.write();
    let tx = connection.transaction().unwrap();
    for number in 0..263 {
        let subject = format!("exec/base-{number}");
        append_claim_record_tx(
            &tx,
            "node",
            &subject,
            "intent.desired",
            None,
            &json!({"subject":subject,"kind":"exec","desired":{"number":number}}),
            &[],
            None,
        )
        .unwrap();
    }
    let malformed = append_claim_record_tx(
        &tx,
        "node",
        "exec/bad",
        "intent.desired",
        None,
        &json!({}),
        &[],
        None,
    )
    .unwrap();
    let repaired = append_claim_record_tx(
        &tx,
        "node",
        "exec/repaired",
        "intent.desired",
        None,
        &json!({"subject":"exec/repaired","kind":"exec","desired":{}}),
        &[],
        None,
    )
    .unwrap();
    tx.execute("INSERT INTO replica_records(record_ref,writer,sequence,envelope_hash,position,raw,state,claim_id,updated_at_unix_ms)
            VALUES ('record/repaired','node',1,'test',0,X'','repaired',?1,'1')", [&repaired.id]).unwrap();
    for number in 0..1500 {
        append_claim_record_tx(
            &tx,
            "node",
            "agent/events",
            "harness.observed",
            None,
            &json!({"fields":{"number":number,"padding":"x".repeat(1024)}}),
            &[],
            None,
        )
        .unwrap();
    }
    for subject in ["glass/hidden", "Glass/visible", "glass-visible"] {
        append_claim_record_tx(
            &tx,
            "node",
            subject,
            "harness.observed",
            None,
            &json!({"fields":{}}),
            &[],
            None,
        )
        .unwrap();
    }
    // Numeric time and batch/position ties cross more than one decode page.
    tx.execute("UPDATE claims SET accepted_at_unix_ms='1'", [])
        .unwrap();
    let expected = tx.prepare("SELECT claims.store_index, claims.subject FROM claims JOIN batches ON batches.id=claims.batch_id
            WHERE substr(claims.subject,1,6)<>'glass/' COLLATE BINARY
            AND claims.id<>?1 ORDER BY claims.store_index").unwrap()
        .query_map([&repaired.id], |row| Ok((row.get::<_,u64>(0)?,row.get::<_,String>(1)?)))
        .unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap();
    let mut progress = Vec::new();
    project_replicated_base_claims_with_progress(&tx, &mut |stage| progress.push(stage)).unwrap();
    assert_eq!(
        progress
            .iter()
            .map(|stage| stage.processed.unwrap())
            .collect::<Vec<_>>(),
        [0, 256, 264]
    );
    assert!(progress.iter().all(|stage| stage.total == Some(264)));
    let actual = tx
        .prepare("SELECT store_index,subject FROM event_positions ORDER BY store_index")
        .unwrap()
        .query_map([], |row| {
            Ok((row.get::<_, u64>(0)?, row.get::<_, String>(1)?))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(actual, expected);
    assert_eq!(
        tx.query_row("SELECT COUNT(*) FROM desired", [], |row| row
            .get::<_, u64>(0))
            .unwrap(),
        263
    );
    let quarantined = tx
        .query_row(
            "SELECT aggregate FROM projection_health WHERE aggregate LIKE 'projection:base:%'",
            [],
            |row| row.get::<_, String>(0),
        )
        .unwrap();
    assert_eq!(quarantined, format!("projection:base:{}", malformed.id));
    // Repeating the same fold retains the same event positions and quarantine identity.
    project_replicated_base_claims_with_progress(&tx, &mut |_| {}).unwrap();
    assert_eq!(
        tx.query_row("SELECT COUNT(*) FROM event_positions", [], |row| row
            .get::<_, usize>(0))
            .unwrap(),
        expected.len()
    );
    tx.commit().unwrap();
}

#[test]
fn base_replay_body_page_work_does_not_grow_with_selected_order_size() {
    let store = Store::open_memory("node").unwrap();
    let mut connection = store.connection.write();
    let tx = connection.transaction().unwrap();
    tx.execute_batch(
        "CREATE TEMP TABLE heal_replay_base_order(
            ordinal INTEGER PRIMARY KEY, claim_id TEXT NOT NULL)",
    )
    .unwrap();
    let mut costs = Vec::new();
    for (start, end) in [(0, 512), (512, 4096)] {
        for number in start..end {
            let subject = format!("exec/page-{number}");
            let claim = append_claim_record_tx(
                &tx,
                "node",
                &subject,
                "intent.desired",
                None,
                &json!({"subject":subject,"kind":"exec","desired":{}}),
                &[],
                None,
            )
            .unwrap();
            tx.execute(
                "INSERT INTO temp.heal_replay_base_order(claim_id) VALUES (?1)",
                [&claim.id],
            )
            .unwrap();
        }
        let mut statement = tx.prepare(BASE_REPLAY_PAGE_SQL).unwrap();
        let page = statement
            .query_map(params![0, 256], claim_from_row)
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(page.len(), 256);
        assert_eq!(page[0].subject, "exec/page-0");
        assert_eq!(page[255].subject, "exec/page-255");
        costs.push((
            statement.get_status(rusqlite::StatementStatus::VmStep),
            statement.get_status(rusqlite::StatementStatus::FullscanStep),
            statement.get_status(rusqlite::StatementStatus::Sort),
        ));
    }
    assert_eq!(
        costs[0], costs[1],
        "body page work must depend on its selected page, not the remaining canonical order"
    );
    assert_eq!(costs[0].1, 0, "ordinal lookup must not scan the order");
    assert_eq!(
        costs[0].2, 0,
        "body pages must not repeat the canonical sort"
    );
    // This measures the actual body-page statement, not the once-per-replay canonical
    // sort, the full runtime fold or writer latency.
    tx.rollback().unwrap();
}

#[test]
fn full_replay_logs_every_guard_and_incremental_error_once() {
    for reason in [
        "missing-health",
        "unhealthy-projection",
        "frontier-ahead",
        "non-incremental-kind",
        "malformed-operation",
        "operation-conflict",
        "incremental-error:internal",
    ] {
        FULL_REPLAYS.with(|count| count.set(0));
        let store = Store::open_memory("node").unwrap();
        store
            .project_replication_backlog_with_log("test/setup", |_| {})
            .unwrap();
        let frontier = store.index().unwrap();
        {
            let mut connection = store.connection.lock().unwrap();
            let transaction = connection.transaction().unwrap();
            let operation = json!({"id":"op/replay-test", "request_digest":"digest-a"});
            let (kind, body) = match reason {
                "non-incremental-kind" => ("work.unknown", json!({"fields":{}})),
                "malformed-operation" => (
                    "harness.observed",
                    json!({"fields":{"state":"ready"}, "_operation":{"id":"op/malformed"}}),
                ),
                "operation-conflict" => (
                    "harness.observed",
                    json!({"fields":{"state":"ready"}, "_operation":operation}),
                ),
                "incremental-error:internal" => ("intent.desired", json!({})),
                _ => ("harness.observed", json!({"fields":{"state":"ready"}})),
            };
            // Seed historical input without pre-projecting it or requiring a current schema.
            let claim = append_claim_record_tx(
                &transaction,
                "node",
                "agent/node.test",
                kind,
                None,
                &body,
                &[],
                None,
            )
            .unwrap();
            if reason == "operation-conflict" {
                transaction.execute("INSERT INTO operations(id, request_digest, canonical_claim_id, state) VALUES ('op/replay-test', 'digest-b', ?1, 'active')", [&claim.id]).unwrap();
            }
            match reason {
                "missing-health" => {
                    transaction
                        .execute("DELETE FROM projection_health WHERE aggregate='graph'", [])
                        .unwrap();
                }
                "unhealthy-projection" => {
                    transaction
                        .execute(
                            "UPDATE projection_health SET status='stale' WHERE aggregate='graph'",
                            [],
                        )
                        .unwrap();
                }
                "frontier-ahead" => {
                    transaction.execute("UPDATE projection_health SET last_good_store_index=999999 WHERE aggregate='graph'", []).unwrap();
                }
                _ => {}
            }
            transaction.commit().unwrap();
        }
        let target = store.index().unwrap();
        let mut lines = Vec::new();
        assert!(
            store
                .project_replication_backlog_with_log(
                    "startup/project-replication-backlog",
                    |line| {
                        // The log sink runs before any full-replay tables are changed.
                        assert_eq!(store.index().unwrap(), target);
                        lines.push(line.to_owned());
                    }
                )
                .unwrap(),
            "{reason}"
        );
        let expected_frontier = match reason {
            "missing-health" => 0,
            "frontier-ahead" => 999999,
            _ => frontier,
        };
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.starts_with("st: projection full replay "))
                .cloned()
                .collect::<Vec<_>>(),
            [format!(
                "st: projection full replay phase=startup/project-replication-backlog reason={reason} frontier={expected_frontier} target={target}"
            )],
            "{reason}"
        );
        let details = lines
            .iter()
            .filter_map(|line| line.strip_prefix("st: projection failure detail "))
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        if matches!(reason, "incremental-error:internal" | "operation-conflict"
            | "malformed-operation" | "non-incremental-kind") {
            assert_eq!(details.len(), 1, "{reason}");
            assert_eq!(details[0]["subject"], "agent/node.test");
            assert!(
                details[0]["claim_id"]
                    .as_str()
                    .is_some_and(|id| id.len() == 64)
            );
            assert_eq!(details[0]["error_truncated"], false);
            if reason == "operation-conflict" {
                assert_eq!(details[0]["operation_id"], "op/replay-test");
                assert!(details[0]["error"].as_str().unwrap().contains("digest_matches=false"));
            } else if reason == "incremental-error:internal" {
                assert!(
                    details[0]["error"]
                        .as_str()
                        .unwrap()
                        .contains("missing field")
                );
            } else {
                assert_eq!(details[0]["stage"], "incremental-guard");
                assert_eq!(details[0]["code"], reason);
                let expected_kind = match reason {
                    "malformed-operation" => "harness.observed",
                    _ => "work.unknown",
                };
                assert_eq!(details[0]["claim_kind"], expected_kind);
                let detail_position = lines.iter().position(|line|
                    line.starts_with("st: projection failure detail ")).unwrap();
                let replay_position = lines.iter().position(|line|
                    line.starts_with("st: projection full replay ")).unwrap();
                assert!(detail_position < replay_position);
            }
        } else {
            assert!(details.is_empty(), "{reason}");
        }
        assert_eq!(
            FULL_REPLAYS.with(std::cell::Cell::get),
            2,
            "one setup replay and one fallback: {reason}"
        );
        let digest = store
            .replication_status(true, None, &[])
            .unwrap()
            .graph_digest;
        store.replay_replication_graph().unwrap();
        assert_eq!(
            store
                .replication_status(true, None, &[])
                .unwrap()
                .graph_digest,
            digest,
            "{reason}"
        );
        lines.clear();
        assert!(
            store
                .project_replication_backlog_with_log("replication", |line| lines
                    .push(line.to_owned()))
                .unwrap()
        );
        assert!(
            lines.is_empty(),
            "a healthy frontier must not log a fallback"
        );
    }
}

#[test]
fn simple_replication_rejects_structural_and_malformed_operation_claims() {
    assert!(Store::simple_replication_kind("harness.observed"));
    assert!(Store::simple_replication_kind("work.renewed"));
    assert!(Store::simple_replication_kind("work.claimed"));
    assert!(Store::simple_replication_kind("work.progress"));
    assert!(Store::simple_replication_kind(WORK_NUDGED_KIND));
    assert!(!Store::simple_replication_kind("work.unknown"));
    assert!(!Store::simple_replication_kind("mission.published"));
    let store = Store::open_memory("node").unwrap();
    assert!(store.project_replication_backlog().unwrap());
    let mut connection = store.connection.lock().unwrap();
    let transaction = connection.transaction().unwrap();
    append_claim_tx(
        &transaction,
        &store.origin,
        "agent/node.test",
        "harness.observed",
        Some("agent/node.test"),
        &json!({"fields": {"state": "ready"}, "_operation": {"id": "test"}}),
        &[],
        None,
    )
    .unwrap();
    assert!(!try_project_all_simple_replication_tx(&transaction, &store.origin).unwrap());
}

#[test]
fn simple_replication_registers_unambiguous_event_operations() {
    let store = Store::open_memory("node").unwrap();
    assert!(store.project_replication_backlog().unwrap());
    {
        let mut connection = store.connection.lock().unwrap();
        let transaction = connection.transaction().unwrap();
        append_claim_tx(
            &transaction,
            &store.origin,
            "agent/node.test",
            "harness.observed",
            Some("agent/node.test"),
            &json!({"fields": {"state": "ready"}, "_operation": {
                "id": "op/simple-event", "request_digest": "digest-one"
            }}),
            &[],
            None,
        )
        .unwrap();
        assert!(try_project_all_simple_replication_tx(&transaction, &store.origin).unwrap());
        transaction.commit().unwrap();
    }
    assert!(store.operation_projection_drift().unwrap().is_empty());
    assert!(store.project_replication_backlog().unwrap());
    {
        let mut connection = store.connection.lock().unwrap();
        let transaction = connection.transaction().unwrap();
        append_claim_tx(
            &transaction,
            &store.origin,
            "agent/node.test",
            "harness.observed",
            Some("agent/node.test"),
            &json!({"fields": {"state": "idle"}, "_operation": {
                "id": "op/simple-event", "request_digest": "digest-two"
            }}),
            &[],
            None,
        )
        .unwrap();
        assert!(!try_project_all_simple_replication_tx(&transaction, &store.origin).unwrap());
        transaction.commit().unwrap();
    }
    assert!(store.project_replication_backlog().unwrap());
    assert!(store.operation_projection_drift().unwrap().is_empty());
    let connection = store.connection.lock().unwrap();
    assert_eq!(
        operation_tx(&connection, "op/simple-event")
            .unwrap()
            .unwrap()
            .2,
        "conflict"
    );
}

fn rewrite_envelope(
    envelope: &ReplicaEnvelope,
    update: impl FnOnce(&mut ReplicaEnvelopePayload),
) -> ReplicaEnvelope {
    let bytes = envelope.payload.bytes().expect("envelope base64");
    let mut payload: ReplicaEnvelopePayload =
        ciborium::from_reader(bytes).expect("envelope CBOR");
    update(&mut payload);
    let mut bytes = Vec::new();
    ciborium::into_writer(&payload, &mut bytes).expect("updated envelope CBOR");
    ReplicaEnvelope {
        hash: replica_envelope_hash(
            &envelope.writer,
            envelope.sequence,
            envelope.previous_hash.as_deref(),
            envelope.accepted_at_unix_ms,
            &bytes,
        ),
        payload: bytes.into(),
        ..envelope.clone()
    }
}

pub(super) fn receive_and_project(
    target: &Store,
    relay: &str,
    exchange: &ReplicationExchange,
) -> ReplicationAdmission {
    target.bind_fleet(TEST_FLEET).expect("target fleet");
    target
        .receive_replication_exchange(relay, TEST_FLEET, exchange)
        .expect("replication receipt");
    let admission = target
        .validate_replication_backlog()
        .expect("replication admission");
    target
        .apply_replication_repairs()
        .expect("replication repairs");
    assert!(
        target
            .project_replication_backlog()
            .expect("replication projection")
    );
    admission
}

pub(super) fn exchange_from(
    source: &Store,
    remote: &ReplicationInventory,
) -> ReplicationExchange {
    source.bind_fleet(TEST_FLEET).expect("source fleet");
    source
        .export_replication_exchange(TEST_FLEET, remote)
        .expect("replication exchange")
}

fn expire_work_lease_by_claim(store: &Store, subject: &str, actor: &str, incarnation: &str) {
    super::expire_work_lease_by_claim(store, subject, actor, incarnation);
}

fn simple(command: &str) -> NormalizedIntent {
    parse_intent(
        &format!("version 2\n exec \"work\" {{ command {command:?}; restart \"never\" }} "),
        "node",
    )
    .expect("intent")
}

#[test]
fn desired_revision_does_not_depend_on_json_object_insertion_order() {
    let mut left_value = serde_json::Map::new();
    left_value.insert("workspace".into(), Value::String("/workspace".into()));
    left_value.insert("restart".into(), Value::String("always".into()));
    let mut right_value = serde_json::Map::new();
    right_value.insert("restart".into(), Value::String("always".into()));
    right_value.insert("workspace".into(), Value::String("/workspace".into()));
    let left = DesiredSubject {
        accountable: None,
        subject: "agent/example".into(),
        kind: "agent".into(),
        desired: Value::Object(left_value),
        member: None,
        owner_run: Some("mission-run/example".into()),
        owner_generation: Some("run-generation/example".into()),
        owner_step: None,
    };
    let right = DesiredSubject {
        accountable: None,
        desired: Value::Object(right_value),
        ..left.clone()
    };

    assert_eq!(desired_revision(&left), desired_revision(&right));
}

fn publish_mission(store: &Store, source: &str, key: &str) -> MissionSpec {
    let intent = crate::graph::parse_intent(source, "node").expect("mission intent");
    let planned = store
        .mission(
            &intent,
            IntentInput {
                kdl: source.into(),
                source_name: None,
            },
        )
        .expect("mission preview");
    store
        .apply(&intent, &planned.subject_tokens, key)
        .expect("mission publish");
    intent
        .missions
        .values()
        .next()
        .expect("published mission")
        .clone()
}

#[test]
fn mission_root_tree_preserves_state_and_summaries_without_work_enrichment() {
    let store = Store::open_memory("node").unwrap();
    publish_mission(
        &store,
        r#"version 2
mission "summary-root" state="ready" {
  goal "Preserve the complete descendant tree."
  step "child" { agentless }
  loop "rounds" {
    max-rounds 1
    round { completion { when "all-steps-exhausted" } }
  }
}
mission "summary-child" state="ready" {
  goal "Preserve effective states and summaries."
  concurrent-runs max=2
  queue "investigations" {
    assigned-to "agent/tree/worker"
    step "work" { }
    step "after" { }
  }
}
    "#,
        "summary-missions",
    );
    let request = |mission: &str, key: &str| MissionRunRequest {
        mission: mission.into(),
        revision: None,
        workspace: "/tmp".into(),
        requester: Some("person/test".into()),
        mode: Some("run".into()),
        inputs: BTreeMap::new(),
        idempotency_key: key.into(),
    };
    let root = store
        .create_mission_run(&request("summary-root", "summary-root"))
        .unwrap();
    let root_step = &root
        .steps
        .iter()
        .find(|step| step.step == "child")
        .unwrap()
        .subject;
    let child = store
        .create_child_mission_run(
            &request("summary-child", "summary-child"),
            &root,
            root_step,
            None,
        )
        .unwrap();
    let work = &child
        .steps
        .iter()
        .find(|step| step.step == "work")
        .unwrap()
        .subject;
    let grandchild = store
        .create_child_mission_run(
            &request("summary-child", "summary-grandchild"),
            &child,
            work,
            None,
        )
        .unwrap();
    for (kind, summary) in [
        ("work.progress", "selected progress"),
        ("work.submitted", "selected completion"),
    ] {
        store
            .append_claim(&ClaimInput {
                subject: work.clone(),
                kind: kind.into(),
                actor: Some("agent/tree/worker".into()),
                fields: BTreeMap::from([
                    ("attempt".into(), Value::from(1)),
                    ("summary".into(), Value::String(summary.into())),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
    }
    // An expired lease must be interpreted, not copied as held work.
    store
        .connection
        .write()
        .execute(
            "UPDATE step_runs SET status='working', lease_owner='agent/tree/worker',
                 lease_incarnation='expired', lease_expires_at_unix_ms='0' WHERE subject=?1",
            [work],
        )
        .unwrap();
    store
        .set_mission_run_state(&grandchild.id, "completed", "terminal", None)
        .unwrap();
    store
        .append_claim(&ClaimInput {
            subject: root.subject.clone(),
            kind: "reconcile.fault".into(),
            actor: None,
            fields: BTreeMap::from([
                (
                    "scope".into(),
                    Value::String("scheduler/first-readiness".into()),
                ),
                ("status".into(), Value::String("faulted".into())),
                (
                    "reason".into(),
                    Value::String("preserved scheduler fault".into()),
                ),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    let index = store.index().unwrap();
    STEPS_ENRICHED.with(|enriched| enriched.set(0));
    let tree = store.mission_runs_for_root(&root.subject).unwrap();
    assert_eq!(STEPS_ENRICHED.with(std::cell::Cell::get), 0);
    assert_eq!(store.index().unwrap(), index, "tree reads must remain pure");
    assert_eq!(tree.len(), 3, "no descendants may be truncated");
    assert_eq!(
        tree.iter().map(|run| &run.subject).collect::<BTreeSet<_>>(),
        BTreeSet::from([&root.subject, &child.subject, &grandchild.subject])
    );
    assert_eq!(
        serde_json::to_value(store.mission_runs_for_root(&root.id).unwrap()).unwrap(),
        serde_json::to_value(&tree).unwrap(),
        "both root ID forms must work"
    );
    let root_view = tree.iter().find(|run| run.subject == root.subject).unwrap();
    assert!(!root_view.loops.is_empty(), "loop fields must be preserved");
    assert_eq!(
        root_view.scheduler_fault.as_deref(),
        Some("preserved scheduler fault")
    );
    let child_view = tree
        .iter()
        .find(|run| run.subject == child.subject)
        .unwrap();
    // Names sort opposite to queue positions: both fields must survive so the
    // human tree can keep its labels and definition order.
    for (name, position) in [("work", 1), ("after", 2)] {
        let step = child_view
            .steps
            .iter()
            .find(|step| step.step == name)
            .unwrap();
        assert_eq!(step.queue.as_deref(), Some("investigations"));
        assert_eq!(step.queue_position, Some(position));
    }
    let step = child_view
        .steps
        .iter()
        .find(|step| step.step == "work")
        .unwrap();
    assert_eq!(step.status, "ready");
    assert!(step.claimant.is_none());
    assert_eq!(step.progress_summary.as_deref(), Some("selected progress"));
    assert_eq!(
        step.completion_summary.as_deref(),
        Some("selected completion")
    );
    // Compare the old reader's observable tree fields and ordering. Only
    // work-presentation fields deliberately omitted by the summary reader differ.
    let connection = store.readers.get();
    let full = tree
        .iter()
        .map(|run| mission_run_view_tx(&connection, &run.id).unwrap())
        .collect::<Vec<_>>();
    assert!(
        STEPS_ENRICHED.with(std::cell::Cell::get) > 0,
        "old-reader negative control"
    );
    let tree_fields = |views: &[MissionRunView]| {
        let mut value = serde_json::to_value(views).unwrap();
        for run in value.as_array_mut().unwrap() {
            for step in run["steps"].as_array_mut().unwrap() {
                for name in [
                    "execution_started_at_unix_ms",
                    "execution_elapsed_ms",
                    "timeout_extension_ms",
                    "timeout_ms",
                    "wake",
                    "ready_age_ms",
                    "fresh_context",
                    "person_answers",
                    "under",
                ] {
                    step.as_object_mut().unwrap().remove(name);
                }
            }
        }
        value
    };
    assert_eq!(tree_fields(&tree), tree_fields(&full));

    let (first, cursor) = store
        .mission_runs_for_root_page(&root.subject, None, 2)
        .unwrap()
        .unwrap();
    let (last, end) = store
        .mission_runs_for_root_page(&root.subject, cursor.as_deref(), 2)
        .unwrap()
        .unwrap();
    assert!(cursor.is_some());
    assert!(end.is_none());
    assert_eq!(
        serde_json::to_value(first.into_iter().chain(last).collect::<Vec<_>>()).unwrap(),
        serde_json::to_value(&tree).unwrap(),
        "paged human tree preserves every selected state, gate, summary and order"
    );
    assert!(store
        .mission_runs_for_root_page(&root.subject, Some("another-root"), 2)
        .unwrap()
        .is_none());
    assert!(store.mission_runs_for_root_page(&root.subject, None, 0).is_err());
    assert_eq!(store.index().unwrap(), index, "pagination must remain a read");
    let plan = connection
        .prepare(
            "EXPLAIN QUERY PLAN SELECT id FROM mission_runs INDEXED BY mission_runs_root_page_index
                 WHERE root_run_id=?1 AND (created_at_unix_ms,id)>(?2,?3)
                 ORDER BY created_at_unix_ms,id LIMIT 3",
        )
        .unwrap()
        .query_map(params![root.id, "0", "cursor"], |row| row.get::<_, String>(3))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
        .join("\n");
    assert!(plan.contains("mission_runs_root_page_index"), "{plan}");
    assert!(!plan.contains("SCAN") && !plan.contains("TEMP B-TREE"), "{plan}");

    // Only a finished root accepts a user-set outcome. Exercise that separately
    // after checking active descendants and their expired leases above.
    store
        .set_mission_run_state(&root.id, "completed", "terminal", None)
        .unwrap();
    store
        .set_mission_run_outcome(
            &root.id,
            "failed",
            "person/test",
            "preserved outcome",
            "summary-outcome",
        )
        .unwrap();
    let index = store.index().unwrap();
    let finished_tree = store.mission_runs_for_root(&root.id).unwrap();
    assert_eq!(store.index().unwrap(), index);
    let finished = finished_tree.iter().find(|run| run.id == root.id).unwrap();
    assert_eq!(finished.outcome.as_ref().unwrap().reason, "preserved outcome");
    assert_eq!(
        finished.outcome,
        store.mission_run(&root.id).unwrap().unwrap().outcome
    );
}

#[test]
fn mission_root_page_seeks_across_equal_time_ties_and_rejects_retired_cursor() {
    let store = Store::open_memory("node").unwrap();
    publish_mission(
        &store,
        r#"version 2
mission "page-root" state="ready" {
  goal "Show one bounded page of a large root."
  step "work" { agentless }
}
"#,
        "page-root",
    );
    let root = store
        .create_mission_run(&MissionRunRequest {
            mission: "page-root".into(),
            revision: None,
            workspace: "/tmp".into(),
            requester: Some("person/test".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "page-root-run".into(),
        })
        .unwrap();
    // Invented rows isolate equal-time cursor order from admission scheduling.
    // They share a valid revision but have no steps, so hydration stays inexpensive.
    let connection = store.connection.write();
    for index in 0..51 {
        let id = format!("page-child-{index:03}");
        let generation = format!("page-generation-{index:03}");
        connection
            .execute(
                "INSERT INTO mission_runs
                     SELECT ?1,mission_id,initial_revision,?2,root_revision,
                            root_run_id,parent_step_run,workspace,requester,inputs,mode,status,
                            phase,created_at_unix_ms,updated_at_unix_ms
                     FROM mission_runs WHERE id=?3",
                params![id, generation, root.id],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO run_generations
                     SELECT ?1,?2,revision,NULL,status,actor,reason,
                            created_at_unix_ms,updated_at_unix_ms
                     FROM run_generations
                     WHERE id=(SELECT current_generation_id FROM mission_runs WHERE id=?3)",
                params![generation, id, root.id],
            )
            .unwrap();
    }
    drop(connection);
    store
        .connection
        .write()
        .execute(
            "INSERT INTO mission_runs
                 SELECT 'page-other',mission_id,initial_revision,current_generation_id,
                        root_revision,'another-root',parent_step_run,workspace,requester,inputs,
                        mode,status,phase,created_at_unix_ms,updated_at_unix_ms
                 FROM mission_runs WHERE id=?1",
            [&root.id],
        )
        .unwrap();
    assert!(store
        .mission_runs_for_root_page(&root.subject, Some("page-other"), 50)
        .unwrap()
        .is_none());
    let expected = store.mission_runs_for_root(&root.subject).unwrap();
    let (first, cursor) = store
        .mission_runs_for_root_page(&root.subject, None, 50)
        .unwrap()
        .unwrap();
    assert_eq!(first.len(), 50);
    let cursor = cursor.expect("51st run requires continuation");
    let (last, end) = store
        .mission_runs_for_root_page(&root.subject, Some(&cursor), 50)
        .unwrap()
        .unwrap();
    assert_eq!(last.len(), 2);
    assert!(end.is_none());
    assert_eq!(
        first.into_iter().chain(last).map(|run| run.subject).collect::<Vec<_>>(),
        expected.into_iter().map(|run| run.subject).collect::<Vec<_>>()
    );
    let connection = store.connection.write();
    connection
        .execute("DELETE FROM run_generations WHERE run_id=?1", [&cursor])
        .unwrap();
    connection
        .execute("DELETE FROM mission_runs WHERE id=?1", [&cursor])
        .unwrap();
    drop(connection);
    assert!(store
        .mission_runs_for_root_page(&root.subject, Some(&cursor), 50)
        .unwrap()
        .is_none());
}

#[test]
fn loop_execution_clock_uses_only_its_direct_children_and_current_steps() {
    let store = Store::open_memory("node").unwrap();
    publish_mission(
        &store,
        r#"version 2
mission "clock-parent" state="ready" {
  goal "Keep two independent loops open."
  concurrent-runs max=20
  step "first" { agentless }
  step "other" { agentless }
}
mission "clock-worker" state="ready" {
  goal "Start the loop clock when work is claimed."
  concurrent-runs max=20
  step "work" { assigned-to "agent/worker" }
  step "automatic" { agentless }
}
mission "clock-automatic" state="ready" {
  goal "Start an automatic round at creation."
  concurrent-runs max=20
  step "automatic" { agentless }
}
"#,
        "clock-missions",
    );
    let request = |mission: &str, key: &str| MissionRunRequest {
        mission: mission.into(),
        revision: None,
        workspace: "/tmp".into(),
        requester: None,
        mode: None,
        inputs: BTreeMap::new(),
        idempotency_key: key.into(),
    };
    let parent = store
        .create_mission_run(&request("clock-parent", "clock-parent"))
        .unwrap();
    let other_root = store
        .create_mission_run(&request("clock-parent", "other-root"))
        .unwrap();
    let parent_step = &parent
        .steps
        .iter()
        .find(|step| step.step == "first")
        .unwrap()
        .subject;
    let sibling_step = &parent
        .steps
        .iter()
        .find(|step| step.step == "other")
        .unwrap()
        .subject;
    let child = store
        .create_child_mission_run(
            &request("clock-worker", "clock-child"),
            &parent,
            parent_step,
            None,
        )
        .unwrap();
    let sibling = store
        .create_child_mission_run(
            &request("clock-automatic", "clock-sibling"),
            &parent,
            sibling_step,
            None,
        )
        .unwrap();
    let foreign = store
        .create_child_mission_run(
            &request("clock-automatic", "foreign-child"),
            &other_root,
            parent_step,
            None,
        )
        .unwrap();
    for excluded in [&sibling, &foreign] {
        store
            .connection
            .write()
            .execute(
                "UPDATE mission_runs SET created_at_unix_ms='1' WHERE id=?1",
                [&excluded.id],
            )
            .unwrap();
    }
    // A worker round that has not been claimed does not start the clock at creation.
    assert_eq!(
        store
            .loop_first_execution_at(&parent.subject, parent_step)
            .unwrap(),
        None
    );
    let work = child.steps.iter().find(|step| !step.agentless).unwrap();
    let automatic = child.steps.iter().find(|step| step.agentless).unwrap();
    for (index, (step, at)) in [(work, "100"), (work, "90"), (automatic, "80")]
        .into_iter()
        .enumerate()
    {
        let claim = store
            .append_claim(&ClaimInput {
                subject: step.subject.clone(),
                kind: "work.claimed".into(),
                actor: Some("agent/worker".into()),
                fields: BTreeMap::from([("attempt".into(), Value::from(index + 1))]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some(format!("clock-claim-{index}")),
            })
            .unwrap();
        store
            .connection
            .write()
            .execute(
                "UPDATE claims SET accepted_at_unix_ms=?2 WHERE id=?1",
                params![claim.id, at],
            )
            .unwrap();
    }
    STEPS_ENRICHED.with(|enriched| enriched.set(0));
    let (clock, reads) = smallclaims::touched::record(|| {
        store
            .loop_first_execution_at(&parent.subject, parent_step)
            .unwrap()
    });
    assert_eq!(clock, Some(80));
    assert_eq!(
        STEPS_ENRICHED.with(std::cell::Cell::get),
        0,
        "the clock must not read presentation history"
    );
    assert!(reads.contains(&format!("children-of-step:{parent_step}")));
    assert!(reads.contains(&child.subject));
    assert!(reads.contains(&child.generation));
    let generation_change = Change {
        subject: "run-generation/new-round-generation".into(),
        kind: "run-generation.created".into(),
        actor: None,
        body: json!({"fields": {"run": child.subject}}).to_string(),
    };
    assert!(
        crate::incremental::change_keys(&generation_change)
            .iter()
            .any(|key| reads.contains(key)),
        "a successor generation must invalidate the clock"
    );
    assert!(reads.contains(&work.subject));
    assert!(reads.contains(&automatic.subject));
    assert!(!reads.contains(&sibling.subject));
    assert!(!reads.contains(&foreign.subject));
    // Earlier terminal rounds still determine the clock.
    store
        .set_mission_run_state(&child.id, "completed", "terminal", None)
        .unwrap();
    assert_eq!(
        store
            .loop_first_execution_at(&parent.id, parent_step)
            .unwrap(),
        Some(80)
    );
    // Claims on predecessor-generation steps cannot start the current round.
    store
        .connection
        .write()
        .execute(
            "UPDATE step_runs SET generation_id=?2 WHERE run_id=?1",
            params![child.id, generation_id_from_subject(&parent.generation)],
        )
        .unwrap();
    assert_eq!(
        store
            .loop_first_execution_at(&parent.id, parent_step)
            .unwrap(),
        Some(child.created_at_unix_ms)
    );
    // No steps, like entirely agentless steps, means the round starts at creation.
    let empty = store
        .create_child_mission_run(
            &request("clock-automatic", "clock-empty"),
            &parent,
            parent_step,
            None,
        )
        .unwrap();
    store
        .connection
        .write()
        .execute("DELETE FROM step_runs WHERE run_id=?1", [&empty.id])
        .unwrap();
    store
        .connection
        .write()
        .execute(
            "UPDATE mission_runs SET created_at_unix_ms='50' WHERE id=?1",
            [&empty.id],
        )
        .unwrap();
    assert_eq!(
        store
            .loop_first_execution_at(&parent.id, parent_step)
            .unwrap(),
        Some(50)
    );
}

#[test]
fn first_readiness_order_prioritizes_current_pending_generations_then_newest() {
    let store = Store::open_memory("node").unwrap();
    let first = start_agentless_run(&store, "first");
    let second = start_agentless_run(&store, "second");
    let third = start_agentless_run(&store, "third");
    // Fixed timestamps prove both the priority class and numeric newest-first ordering.
    for (run, at) in [(&first, "9"), (&second, "10"), (&third, "11")] {
        store.connection.write().execute(
            "UPDATE mission_runs SET created_at_unix_ms=?1 WHERE id=?2",
            params![at, run.id],
        ).unwrap();
    }
    store.set_step_state(&third.steps[0].subject, "ready", None).unwrap();
    assert_eq!(store.active_mission_run_ids_for_origin("node").unwrap(),
        [second.id.clone(), first.id.clone(), third.id.clone()]);
    store.set_step_state(&second.steps[0].subject, "ready", None).unwrap();
    assert_eq!(store.active_mission_run_ids_for_origin("node").unwrap(),
        [first.id.clone(), third.id.clone(), second.id.clone()]);
    // The order is deterministic even when two creations share the same millisecond.
    store.connection.write().execute("UPDATE mission_runs SET created_at_unix_ms='12'", []).unwrap();
    let ids = store.active_mission_run_ids_for_origin("node").unwrap();
    assert_eq!(ids[0], first.id);
    assert!(ids[1] > ids[2]);
}

#[test]
fn active_mission_evaluation_selects_only_the_creating_origin() {
    let store = Store::open_memory("node").unwrap();
    publish_mission(
        &store,
        r#"version 2
mission "origin-owned" state="ready" {
  goal "Evaluate this run on exactly one origin."
  step "work" { agentless }
}
"#,
        "publish-origin-owned",
    );
    let run = store
        .create_mission_run(&MissionRunRequest {
            mission: "origin-owned".into(),
            revision: None,
            workspace: "/tmp".into(),
            requester: Some("person/test".into()),
            mode: None,
            inputs: BTreeMap::new(),
            idempotency_key: "origin-owned-run".into(),
        })
        .unwrap();

    assert_eq!(
        store
            .active_mission_runs_for_origin("node")
            .unwrap()
            .iter()
            .map(|candidate| candidate.subject.as_str())
            .collect::<Vec<_>>(),
        [run.subject.as_str()]
    );
    let evaluation = store.active_mission_runs_for_origin("node").unwrap();
    let presentation = store.mission_run(&run.id).unwrap().unwrap();
    let headers = store.mission_run_headers().unwrap();
    assert_eq!(headers.len(), 1);
    assert_eq!(
        store.mission_for_run(&run.subject).unwrap(),
        Some(run.mission.clone())
    );
    assert_eq!(headers[0].subject, presentation.subject);
    assert_eq!(headers[0].mission, presentation.mission);
    assert_eq!(headers[0].generation, presentation.generation);
    assert_eq!(headers[0].revision, presentation.revision);
    assert_eq!(headers[0].status, presentation.status);
    assert!(headers[0].steps.is_empty());
    assert_eq!(evaluation[0].steps.len(), 1);
    assert_eq!(evaluation[0].steps[0].status, presentation.steps[0].status);
    assert_eq!(
        evaluation[0].steps[0].definition_hash,
        presentation.steps[0].definition_hash
    );
    assert_eq!(
        evaluation[0].steps[0].timeout_ms,
        presentation.steps[0].timeout_ms
    );
    assert_eq!(evaluation[0].steps[0].queue, presentation.steps[0].queue);
    assert!(
        store
            .active_mission_runs_for_origin("other")
            .unwrap()
            .is_empty()
    );
}

#[test]
fn exact_terminal_reason_replaces_only_a_provisional_root_cancellation() {
    let store = Store::open_memory("node").unwrap();
    let mission = publish_mission(
        &store,
        r#"version 2
mission "terminal-replay" state="ready" {
  goal "Keep terminal mission projection deterministic."
  step "provisional" { agentless }
  step "finished" { agentless }
}
"#,
        "publish-terminal-replay",
    );
    let run = store
        .create_mission_run(&MissionRunRequest {
            mission: mission.id,
            revision: Some(mission.revision),
            workspace: "/tmp".into(),
            requester: Some("person/test".into()),
            mode: None,
            inputs: BTreeMap::new(),
            idempotency_key: "terminal-replay-run".into(),
        })
        .unwrap();
    let generation = generation_id_from_subject(&run.generation);

    let mut connection = store.connection.lock().unwrap();
    let transaction = connection.transaction().unwrap();
    transaction
        .execute(
            "UPDATE step_runs SET status='cancelled', blocked_reason='the root mission run is terminal' WHERE generation_id=?1 AND step_path='provisional'",
            [generation],
        )
        .unwrap();
    transaction
        .execute(
            "UPDATE step_runs SET status='completed', blocked_reason='preserved completion' WHERE generation_id=?1 AND step_path='finished'",
            [generation],
        )
        .unwrap();
    terminalize_projected_generation_steps_tx(
        &transaction,
        generation,
        "the exact supersede reason",
        now_ms(),
    )
    .unwrap();
    transaction.commit().unwrap();

    let projected = connection
        .prepare(
            "SELECT step_path, status, blocked_reason FROM step_runs WHERE generation_id=?1 ORDER BY step_path",
        )
        .unwrap()
        .query_map([generation], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(
        projected,
        vec![
            (
                "finished".into(),
                "completed".into(),
                Some("preserved completion".into())
            ),
            (
                "provisional".into(),
                "cancelled".into(),
                Some("the exact supersede reason".into())
            ),
        ]
    );
}

#[test]
fn an_embedded_loop_mission_cannot_start_without_its_parent() {
    let store = Store::open_memory("node").unwrap();
    let source = r#"
version 2
mission "root" state="ready" {
  goal "Own one embedded loop mission."
  loop "work" {
    max-rounds 1
    round { completion { when "all-steps-exhausted" } }
  }
}
"#;
    let intent = crate::graph::parse_intent(source, "node").unwrap();
    let internal = intent.missions["root"].steps["work"]
        .loop_spec
        .as_ref()
        .unwrap()
        .round
        .id
        .clone();
    let planned = store
        .mission(
            &intent,
            IntentInput {
                kdl: source.into(),
                source_name: None,
            },
        )
        .unwrap();
    store
        .apply(&intent, &planned.subject_tokens, "publish-loop-parent")
        .unwrap();

    let error = store
        .create_mission_run(&MissionRunRequest {
            mission: internal,
            revision: None,
            workspace: "/tmp".into(),
            requester: Some("person/test".into()),
            mode: None,
            inputs: BTreeMap::new(),
            idempotency_key: "direct-internal-run".into(),
        })
        .unwrap_err();

    assert_eq!(error.code, "internal-mission");
}

#[test]
fn eval_cleanup_includes_descendant_mission_runtimes() {
    let store = Store::open_memory("node").unwrap();
    let source = r#"
version 2

mission "eval/root" state="ready" timeout="1m" {
  goal "Own one isolated eval tree."
}

mission "eval/child" state="ready" {
  goal "Run one child agent."
}
"#;
    publish_mission(&store, source, "publish-eval-tree");
    let root = store
        .create_mission_run(&MissionRunRequest {
            mission: "eval/root".into(),
            revision: None,
            workspace: "/eval".into(),
            requester: Some("person/test".into()),
            mode: Some("eval".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "eval-tree-root".into(),
        })
        .unwrap();
    let mut seat = crate::graph::parse_intent(
        r#"version 2
agent "eval/root/seat" { workspace "/eval"; command "true"; restart "never" }
"#,
        "node",
    )
    .unwrap();
    seat.subjects
        .get_mut("agent/eval/root/seat")
        .unwrap()
        .owner_run = Some(root.subject.clone());
    store
        .apply_internal(&seat, "declare-eval-root-seat")
        .unwrap();
    let child = store
        .create_child_mission_run(
            &MissionRunRequest {
                mission: "eval/child".into(),
                revision: None,
                workspace: "/eval/child".into(),
                requester: Some("daemon/test".into()),
                mode: Some("run".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "eval-tree-child".into(),
            },
            &root,
            "step-run/subscription/root/reviews",
            None,
        )
        .unwrap();
    let declaration = crate::graph::parse_execution_intent(
        r#"version 2
agent "worker" { workspace "/eval/child"; command "true"; restart "never" }
"#,
        "node",
        &child.id,
    )
    .unwrap();
    store
        .apply_internal(&declaration, "declare-eval-child-worker")
        .unwrap();

    let runtime_records = store.eval_runtime_records(&root.subject).unwrap();
    assert!(runtime_records.contains(&(format!("{}.worker", child.id), true)));
    assert!(runtime_records.contains(&("eval.root.seat".into(), true)));
    assert!(
        store
            .desired_subjects()
            .unwrap()
            .iter()
            .any(|desired| desired.owner_run.as_deref() == Some(child.subject.as_str()))
    );

    assert!(
        store
            .retire_eval_owned_desired(&root.subject)
            .unwrap()
            .is_empty()
    );
    assert!(store.desired_subjects().unwrap().iter().all(|desired| {
        desired.owner_run.as_deref() != Some(root.subject.as_str())
            && desired.owner_run.as_deref() != Some(child.subject.as_str())
    }));
}

#[test]
fn the_store_requires_bounded_eval_deadlines_and_keeps_them_optional_elsewhere() {
    let store = Store::open_memory("node").unwrap();
    publish_mission(
        &store,
        r#"
version 2

mission "eval/missing-deadline" state="ready" {
  goal "Reject an eval without a deadline."
}

mission "eval/long-deadline" state="ready" timeout="21m" {
  goal "Reject an eval with a deadline above the limit."
}

mission "ordinary" state="ready" {
  goal "Allow an ordinary mission without a deadline."
}

mission "bounded" state="ready" timeout="1s" {
  goal "Store one absolute deadline for an ordinary mission."
}
"#,
        "publish-deadline-missions",
    );
    let request = |mission: &str, mode: &str| MissionRunRequest {
        mission: mission.into(),
        revision: None,
        workspace: "/work".into(),
        requester: Some("person/test".into()),
        mode: Some(mode.into()),
        inputs: BTreeMap::new(),
        idempotency_key: format!("deadline-{mission}-{mode}"),
    };

    assert_eq!(
        store
            .create_mission_run(&request("eval/missing-deadline", "eval"))
            .unwrap_err()
            .code,
        "missing-eval-timeout"
    );
    assert_eq!(
        store
            .create_mission_run(&request("eval/long-deadline", "eval"))
            .unwrap_err()
            .code,
        "eval-timeout-too-large"
    );

    let ordinary = store
        .create_mission_run(&request("ordinary", "run"))
        .unwrap();
    assert_eq!(ordinary.timeout_ms, None);
    assert_eq!(ordinary.deadline_at_unix_ms, None);

    let bounded = store
        .create_mission_run(&request("bounded", "run"))
        .unwrap();
    assert_eq!(bounded.timeout_ms, Some(1_000));
    assert_eq!(
        bounded.deadline_at_unix_ms,
        Some(bounded.created_at_unix_ms + 1_000)
    );
}

#[test]
fn a_declarative_resource_refresh_emits_schema_valid_observer_state() {
    let store = Store::open_memory("node").unwrap();
    let source = r#"version 2
resource "refresh/file" { kind "filesystem.file" }
observer "refresh/file" {
  resource "resource/refresh/file"
  provider "local.file"
  locator "/tmp/st3-refresh-test"
  field "status"
}
"#;
    let intent = crate::graph::parse_execution_intent(source, "node", "refresh-run").unwrap();
    let observer = intent
        .subjects
        .values()
        .find(|subject| subject.kind == "observer")
        .unwrap()
        .subject
        .clone();
    store
        .apply_internal(&intent, "declare-refresh-observer")
        .unwrap();

    let refresh = r#"version 2
resource "refresh/file" {
  refresh "manual" { timeout "1s" }
}
"#;
    let intent = crate::graph::parse_intent(refresh, "node").unwrap();
    let planned = store
        .mission(
            &intent,
            IntentInput {
                kdl: refresh.into(),
                source_name: None,
            },
        )
        .unwrap();
    store
        .apply_as(
            &intent,
            &planned.subject_tokens,
            "request-resource-refresh",
            Some("person/operator"),
        )
        .unwrap();

    let state = store
        .latest_claim(&observer, Some("observer.refresh-requested"))
        .unwrap()
        .unwrap();
    assert!(state.body["fields"]["attempt"].as_str().is_some());
}

#[test]
fn state_transition_claims_replace_their_previous_fields() {
    let store = Store::open_memory("node").unwrap();
    store
        .append_claim(&ClaimInput {
            subject: "observer/recovery".into(),
            kind: "observer.state".into(),
            actor: None,
            fields: BTreeMap::from([
                ("state".into(), Value::String("unreachable".into())),
                ("reason".into(), Value::String("the provider failed".into())),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    store
        .append_claim(&ClaimInput {
            subject: "observer/recovery".into(),
            kind: "observer.state".into(),
            actor: None,
            fields: BTreeMap::from([("state".into(), Value::String("healthy".into()))]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();

    let actual = store
        .latest_actual_value("observer/recovery")
        .unwrap()
        .unwrap();
    assert_eq!(actual["state"], "healthy");
    assert!(actual.get("reason").is_none());
}

#[test]
fn observer_completion_fences_roll_back_facts_state_and_idempotency() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    for failure in [false, true] {
        for (accepted_checks, stale_revision) in [(0, false), (1, false), (2, true)] {
            let store = Store::open_memory("node").unwrap();
            let intent = crate::graph::parse_execution_intent(r#"version 2
resource "fenced/file" { kind "filesystem.file" }
observer "fenced/file" {
  resource "resource/fenced/file"
  provider "local.file"
  locator "/tmp/st3-fenced-file"
  field "status"
}
"#, "node", "fenced-run").unwrap();
            store.apply_internal(&intent, "fenced-observer").unwrap();
            let observer = intent.subjects.values().find(|subject| subject.kind == "observer")
                .unwrap().subject.as_str();
            let resource = intent.subjects.values().find(|subject| subject.kind == "resource")
                .unwrap().subject.as_str();
            let revision = store.selected_desired_revision(observer).unwrap().unwrap();
            let selected = if stale_revision { "stale-revision" } else { &revision };
            let input = ClaimInput {
                subject: observer.into(), kind: "observer.state".into(), actor: None,
                fields: BTreeMap::from([
                    ("state".into(), json!("unreachable")),
                    ("revision".into(), json!(revision)),
                    ("attempt".into(), json!("fenced-attempt")),
                    ("reason".into(), json!("provider failed")),
                ]),
                evidence: Vec::new(), expected_subject: None,
                idempotency_key: Some("fenced-failure".into()),
            };
            let before = store.index().unwrap();
            let counts = || {
                let connection = store.readers.get();
                ["claims", "batches", "operations", "idempotency"].map(|table| {
                    connection.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| row.get::<_, u64>(0)).unwrap()
                })
            };
            let before_counts = counts();
            let checks = AtomicUsize::new(0);
            let current = || checks.fetch_add(1, Ordering::SeqCst) < accepted_checks;
            let result = if failure {
                store.append_observer_state(&input, selected, &current).map(|_| ())
            } else {
                store.record_resource_observation(
                    observer, selected, Some("fenced-attempt"), resource, Some("cursor"),
                    &json!({"status": "ready"}), 10, &[], Some(&current),
                ).map(|_| ())
            };
            assert_eq!(result.unwrap_err().code, if stale_revision {
                "stale-observer-revision"
            } else {
                "observer-operation-retired"
            });
            assert_eq!(checks.load(Ordering::SeqCst), if stale_revision { 1 } else { accepted_checks + 1 });
            assert_eq!(store.index().unwrap(), before);
            assert_eq!(counts(), before_counts);
            assert!(store.latest_actual_value(resource).unwrap().is_none());
            assert!(store.latest_actual_value(observer).unwrap().is_none());

            // Rejection must not poison the request's idempotency or operation cache.
            if failure {
                let receipt = store.append_observer_state(&input, &revision, &|| true).unwrap();
                assert_eq!(store.latest_actual_value(observer).unwrap().unwrap()["state"], "unreachable");
                let retry = store.append_observer_state(&input, &revision, &|| true).unwrap();
                assert_eq!(retry.id, receipt.id);
            } else {
                let outcome = store.record_resource_observation(
                    observer, &revision, Some("fenced-attempt"), resource, Some("cursor"),
                    &json!({"status": "ready"}), 10, &[], Some(&|| true),
                ).unwrap();
                assert!(outcome.baseline);
                assert_eq!(store.latest_actual_value(resource).unwrap().unwrap()["facts"]["status"], "ready");
                assert_eq!(store.latest_actual_value(observer).unwrap().unwrap()["state"], "healthy");
            }
        }
    }
}

#[test]
fn unchanged_scheduled_observations_do_not_append_claims() {
    let store = Store::open_memory("node").unwrap();
    let source = r#"version 2
resource "quiet/file" { kind "filesystem.file" }
observer "quiet/file" {
  resource "resource/quiet/file"
  provider "local.file"
  locator "/tmp/st3-quiet-file"
  field "status"
}
"#;
    let intent = crate::graph::parse_execution_intent(source, "node", "quiet-run").unwrap();
    let observer = intent
        .subjects
        .values()
        .find(|subject| subject.kind == "observer")
        .unwrap()
        .subject
        .clone();
    let resource = intent
        .subjects
        .values()
        .find(|subject| subject.kind == "resource")
        .unwrap()
        .subject
        .clone();
    store.apply_internal(&intent, "quiet-observer").unwrap();
    let revision = store.selected_desired_revision(&observer).unwrap().unwrap();
    let facts = json!({"status": "ready"});
    store.record_resource_observation(&observer, &revision, None, &resource, None, &facts, 1, &[], None)
        .unwrap();
    let before = store.index().unwrap();

    let outcome = store.record_resource_observation(&observer, &revision, None, &resource, None, &facts, 2, &[], None)
        .unwrap();

    assert_eq!(store.index().unwrap(), before);
    assert!(outcome.observation_claim.is_none());
    assert!(outcome.changed_fields.is_empty());
}

#[test]
fn manual_refresh_records_an_unchanged_receipt() {
    let store = Store::open_memory("node").unwrap();
    let source = r#"version 2
resource "manual/file" { kind "filesystem.file" }
observer "manual/file" {
  resource "resource/manual/file"
  provider "local.file"
  locator "/tmp/st3-manual-file"
  field "status"
}
"#;
    let intent = crate::graph::parse_execution_intent(source, "node", "manual-run").unwrap();
    let observer = intent
        .subjects
        .values()
        .find(|subject| subject.kind == "observer")
        .unwrap()
        .subject
        .clone();
    let resource = intent
        .subjects
        .values()
        .find(|subject| subject.kind == "resource")
        .unwrap()
        .subject
        .clone();
    store.apply_internal(&intent, "manual-observer").unwrap();
    let revision = store.selected_desired_revision(&observer).unwrap().unwrap();
    let facts = json!({"status": "ready"});
    store.record_resource_observation(&observer, &revision, None, &resource, None, &facts, 1, &[], None)
        .unwrap();

    store.record_resource_observation(&observer, &revision, Some("attempt-one"), &resource, Some("cursor-one"), &facts, 2, &[], None)
        .unwrap();

    let receipt = store
        .latest_claim(&observer, Some("observer.observed"))
        .unwrap()
        .unwrap();
    assert_eq!(receipt.body["fields"]["attempt"], "attempt-one");
    assert_eq!(receipt.body["fields"]["changed"], false);
}

#[test]
fn observer_refresh_requests_complete_in_order() {
    let store = Store::open_memory("node").unwrap();
    let source = r#"version 2
resource "ordered/file" { kind "filesystem.file" }
observer "ordered/file" {
  resource "resource/ordered/file"
  provider "local.file"
  locator "/tmp/st3-ordered-file"
  field "status"
}
"#;
    let intent = crate::graph::parse_execution_intent(source, "node", "ordered-run").unwrap();
    let observer = intent
        .subjects
        .values()
        .find(|subject| subject.kind == "observer")
        .unwrap()
        .subject
        .clone();
    let resource = intent
        .subjects
        .values()
        .find(|subject| subject.kind == "resource")
        .unwrap()
        .subject
        .clone();
    store.apply_internal(&intent, "ordered-observer").unwrap();
    let revision = store.selected_desired_revision(&observer).unwrap().unwrap();
    for attempt in ["attempt-one", "attempt-two"] {
        store
            .append_claim(&ClaimInput {
                subject: observer.clone(),
                kind: "observer.refresh-requested".into(),
                actor: None,
                fields: BTreeMap::from([
                    ("attempt".into(), Value::String(attempt.into())),
                    ("revision".into(), Value::String(revision.clone())),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
    }
    assert_eq!(
        store.pending_observer_refresh_attempt(&observer).unwrap(),
        Some("attempt-one".into())
    );

    store.record_resource_observation(&observer, &revision, Some("attempt-one"), &resource, None, &json!({"status": "ready"}), 1, &[], None)
        .unwrap();
    assert_eq!(
        store.pending_observer_refresh_attempt(&observer).unwrap(),
        Some("attempt-two".into())
    );
    store
        .append_claim(&ClaimInput {
            subject: observer.clone(),
            kind: "observer.state".into(),
            actor: None,
            fields: BTreeMap::from([
                ("attempt".into(), Value::String("attempt-two".into())),
                (
                    "reason".into(),
                    Value::String("provider unavailable".into()),
                ),
                ("revision".into(), Value::String(revision)),
                ("state".into(), Value::String("unreachable".into())),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    assert_eq!(
        store.pending_observer_refresh_attempt(&observer).unwrap(),
        None
    );
}

#[test]
fn common_reads_do_not_wait_for_the_writer_mutex() {
    let store = Arc::new(Store::open_memory("node").unwrap());
    let writer = store.connection.lock().unwrap();
    let other = store.clone();
    let (send, receive) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        send.send((other.index().unwrap(), other.status(None).unwrap()))
            .unwrap();
    });

    let (index, status) = receive
        .recv_timeout(std::time::Duration::from_secs(1))
        .expect("a common read must not wait for the writer mutex");
    assert_eq!(index, 0);
    assert_eq!(status.store_index, 0);
    drop(writer);
}

#[test]
fn repeated_agent_snapshot_does_not_wait_for_busy_read_connections() {
    let store = Arc::new(Store::open_memory("node").unwrap());
    let snapshot = store.index().unwrap();
    store
        .status_for_subject_prefix_at(
            "agent/",
            Some(snapshot),
            true,
            ClaimsReduction::Summary,
        )
        .unwrap();

    let (ready_send, ready_recv) = std::sync::mpsc::channel();
    let (release_send, release_recv) = std::sync::mpsc::channel();
    let holder = store.clone();
    let held = std::thread::spawn(move || {
        holder.hold_read_connections_for_test(|| {
            ready_send.send(()).unwrap();
            release_recv.recv().unwrap();
        });
    });
    ready_recv.recv().unwrap();
    let reader = store.clone();
    let (result_send, result_recv) = std::sync::mpsc::channel();
    let read = std::thread::spawn(move || {
        result_send
            .send(reader.status_for_subject_prefix_at(
                "agent/",
                Some(snapshot),
                true,
                ClaimsReduction::Summary,
            ))
            .unwrap();
    });
    let result = result_recv.recv_timeout(std::time::Duration::from_millis(250));
    release_send.send(()).unwrap();
    held.join().unwrap();
    read.join().unwrap();
    assert_eq!(result.unwrap().unwrap().store_index, snapshot);
}

#[test]
fn bounded_status_reductions_select_only_relevant_subjects() {
    let store = Store::open_memory("node").unwrap();
    let observe = |subject: &str, runtime_id: &str| {
        store
            .append_claim(&ClaimInput {
                subject: subject.into(),
                kind: "runtime.observed".into(),
                actor: None,
                fields: BTreeMap::from([
                    ("runtime_id".into(), Value::String(runtime_id.into())),
                    ("status".into(), Value::String("running".into())),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap()
            .store_index
    };
    let first_index = observe("agent/selected", "selected");
    observe("exec/also-runtime", "also-runtime");
    store
        .append_claim(&ClaimInput {
            subject: "host/unrelated".into(),
            kind: "transport.observed".into(),
            actor: None,
            fields: BTreeMap::from([
                ("protocol".into(), Value::String("test".into())),
                ("status".into(), Value::String("up".into())),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();

    let agents = store
        .status_for_subject_prefix_at("agent/", None, true, ClaimsReduction::Summary)
        .unwrap();
    assert_eq!(
        agents
            .subjects
            .iter()
            .map(|subject| subject.subject.as_str())
            .collect::<Vec<_>>(),
        ["agent/selected"]
    );

    let runtimes = store
        .status_for_claim_kind_at("runtime.observed", None, true, ClaimsReduction::Summary)
        .unwrap();
    assert_eq!(
        runtimes
            .subjects
            .iter()
            .map(|subject| subject.subject.as_str())
            .collect::<Vec<_>>(),
        ["agent/selected", "exec/also-runtime"]
    );

    let first_snapshot = store
        .status_for_claim_kind_at(
            "runtime.observed",
            Some(first_index),
            true,
            ClaimsReduction::Summary,
        )
        .unwrap();
    assert_eq!(first_snapshot.store_index, first_index);
    assert_eq!(first_snapshot.subjects.len(), 1);
    assert_eq!(first_snapshot.subjects[0].subject, "agent/selected");

    observe("agent/new", "new");
    let current_agents = store
        .status_for_subject_prefix_at("agent/", None, true, ClaimsReduction::Summary)
        .unwrap();
    assert_eq!(current_agents.subjects.len(), 2);
    let old_agents = store
        .status_for_subject_prefix_at(
            "agent/",
            Some(first_index),
            true,
            ClaimsReduction::Summary,
        )
        .unwrap();
    assert_eq!(old_agents.subjects.len(), 1);

    for number in 0..80 {
        observe(&format!("agent/bulk-{number:02}"), "bulk");
    }
    let bulk = store
        .status_for_subject_prefix_at("agent/", None, true, ClaimsReduction::Summary)
        .unwrap();
    assert_eq!(bulk.subjects.len(), 82);
    assert!(
        bulk.subjects
            .windows(2)
            .all(|pair| pair[0].subject < pair[1].subject)
    );
    assert_eq!(
        store
            .status_for_subject_prefix_at(
                "agent/",
                Some(first_index),
                true,
                ClaimsReduction::Summary,
            )
            .unwrap()
            .subjects
            .len(),
        1
    );
}

#[test]
fn an_exited_gate_runtime_leaves_the_current_runtime_list() {
    let store = Store::open_memory("node").unwrap();
    let subject = "gate-operation/step-run.run.check/0123456789abcdef01234567";
    for status in ["running", "exited"] {
        store
            .append_claim(&ClaimInput {
                subject: subject.into(),
                kind: "runtime.observed".into(),
                actor: None,
                fields: BTreeMap::from([
                    ("status".into(), Value::String(status.into())),
                    (
                        "runtime_id".into(),
                        Value::String(subject.replace('/', ".")),
                    ),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
    }
    let current = store
        .status_for_claim_kind_at(
            "runtime.observed",
            None,
            false,
            ClaimsReduction::Summary,
        )
        .unwrap();
    assert!(
        current.subjects.iter().all(|item| item.subject != subject),
        "{:?}",
        current.subjects
    );
    let history = store
        .status_for_claim_kind_at("runtime.observed", None, true, ClaimsReduction::Summary)
        .unwrap();
    let gate = history
        .subjects
        .iter()
        .find(|item| item.subject == subject)
        .unwrap();
    assert_eq!(gate.projection.layer, "history");
    assert_eq!(gate.projection.reasons, ["stopped"]);
}

fn observe_runtime(store: &Store, subject: &str, status: &str, incarnation: &str) {
    store
        .append_claim(&ClaimInput {
            subject: subject.into(),
            kind: "runtime.observed".into(),
            actor: None,
            fields: BTreeMap::from([
                ("status".into(), Value::String(status.into())),
                (
                    "runtime_id".into(),
                    Value::String(subject.replace('/', ".")),
                ),
                ("incarnation_id".into(), Value::String(incarnation.into())),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
}

fn current_runtimes(store: &Store) -> (Vec<String>, usize) {
    SUBJECT_REDUCTIONS.with(|reductions| reductions.set(0));
    let view = store
        .status_for_claim_kind_at(
            "runtime.observed",
            None,
            false,
            ClaimsReduction::Summary,
        )
        .unwrap();
    (
        view.subjects
            .into_iter()
            .map(|status| status.subject)
            .collect(),
        SUBJECT_REDUCTIONS.with(std::cell::Cell::get),
    )
}

#[test]
fn agent_card_status_cache_reduces_only_changed_subjects_and_keeps_old_cuts() {
    let store = Store::open_memory("node").unwrap();
    observe_runtime(&store, "agent/cache/first", "running", "one");
    observe_runtime(&store, "agent/cache/second", "running", "two");
    let first_cut = store.index().unwrap();
    let view = |at| serde_json::to_value(store.agent_card_status_at(None, at, true).unwrap()).unwrap();
    let original = view(first_cut);
    SUBJECT_REDUCTIONS.with(|count| count.set(0));
    assert_eq!(view(first_cut), original);
    assert_eq!(SUBJECT_REDUCTIONS.with(std::cell::Cell::get), 0);
    observe_runtime(&store, "exec/unrelated", "running", "other");
    SUBJECT_REDUCTIONS.with(|count| count.set(0));
    let newer = view(store.index().unwrap());
    assert_eq!(newer["subjects"], original["subjects"]);
    assert_eq!(SUBJECT_REDUCTIONS.with(std::cell::Cell::get), 0);
    observe_runtime(&store, "agent/cache/first", "stopped", "three");
    SUBJECT_REDUCTIONS.with(|count| count.set(0));
    let changed = view(store.index().unwrap());
    assert_eq!(SUBJECT_REDUCTIONS.with(std::cell::Cell::get), 1);
    assert_eq!(view(first_cut), original, "a newer entry must not serve an old cut");
    store.forget_current_views();
    assert_eq!(view(store.index().unwrap()), changed, "cache and fresh reducer must agree");
}

#[test]
fn agent_card_and_full_status_caches_never_exchange_harness_or_provenance() {
    let store = Store::open_memory("node").unwrap();
    let subject = "agent/cache/modes";
    observe_runtime(&store, subject, "running", "one");
    store.append_claim(&ClaimInput {
        subject: subject.into(), kind: "harness.observed".into(), actor: Some(subject.into()),
        fields: BTreeMap::from([
            ("state".into(), json!("idle")), ("incarnation_id".into(), json!("one")),
            ("driver".into(), json!("claude")),
        ]), evidence: Vec::new(), expected_subject: None, idempotency_key: None,
    }).unwrap();
    let cut = store.index().unwrap();
    let card = store.agent_card_status_at(None, cut, true).unwrap();
    assert!(card.subjects[0].harness.is_none());
    assert_eq!(card.subjects[0].claims.len(), 1);
    let full = store.status_for_subject_prefix_at("agent/", Some(cut), true, ClaimsReduction::Full).unwrap();
    assert!(full.subjects[0].harness.is_some());
    assert_eq!(full.subjects[0].claims.len(), 2);
    SUBJECT_REDUCTIONS.with(|count| count.set(0));
    let again = store.agent_card_status_at(None, cut, true).unwrap();
    assert_eq!(serde_json::to_value(again).unwrap(), serde_json::to_value(card).unwrap());
    assert_eq!(SUBJECT_REDUCTIONS.with(std::cell::Cell::get), 0);
}

#[test]
fn terminal_resource_status_never_warms_full_harness_or_provenance() {
    let store = Store::open_memory("node").unwrap();
    let subject = "agent/terminal-summary";
    observe_runtime(&store, subject, "running", "one");
    for _ in 0..20 {
        store
            .append_claim(&ClaimInput {
                subject: subject.into(),
                kind: "harness.observed".into(),
                actor: Some(subject.into()),
                fields: BTreeMap::from([
                    ("state".into(), json!("idle")),
                    ("incarnation_id".into(), json!("one")),
                    ("driver".into(), json!("claude")),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
    }
    let cut = store.index().unwrap();
    let lean = store.terminal_resource_status_at(None, cut, true).unwrap();
    assert_eq!(lean.subjects.len(), 1);
    assert!(lean.subjects[0].harness.is_none());
    assert_eq!(lean.subjects[0].claims.len(), 1);
    assert!(
        store
            .smalltalk
            .subject_cache
            .lock()
            .unwrap()
            .statuses
            .is_empty()
    );
    SUBJECT_REDUCTIONS.with(|count| count.set(0));
    store.terminal_resource_status_at(None, cut, true).unwrap();
    assert_eq!(SUBJECT_REDUCTIONS.with(std::cell::Cell::get), 0);
    let full = store
        .status_for_claim_kind_at("runtime.observed", Some(cut), true, ClaimsReduction::Full)
        .unwrap();
    assert!(full.subjects[0].harness.is_some());
    assert!(full.subjects[0].claims.len() > 1);
    observe_runtime(&store, subject, "stopped", "two");
    let newer = store
        .terminal_resource_status_at(None, store.index().unwrap(), true)
        .unwrap();
    assert_eq!(
        newer.subjects[0].actual.as_ref().unwrap()["status"],
        "stopped"
    );
    assert_eq!(
        serde_json::to_value(store.terminal_resource_status_at(None, cut, true).unwrap())
            .unwrap(),
        serde_json::to_value(lean).unwrap()
    );
}

#[test]
fn agent_card_status_cache_has_a_fixed_entry_cap_and_repair_clears_it() {
    let store = Store::open_memory("node").unwrap();
    observe_runtime(&store, "agent/cache/cold", "running", "one");
    let cut = store.index().unwrap();
    let first = store.agent_card_status_at(None, cut, true).unwrap();
    {
        let mut cache = store.smalltalk.subject_cache.lock().unwrap();
        cache.card_statuses.clear();
        for n in 0..AGENT_CARD_STATUS_LIMIT {
            cache.card_statuses.insert(format!("agent/fill/{n}"), StatusEntry {
                read_at: cut, owners: Vec::new(), claims: ClaimsReduction::AgentCard,
                status: first.subjects[0].clone(), action: None,
            });
        }
    }
    let cold = store.agent_card_status_at(None, cut, true).unwrap();
    assert_eq!(serde_json::to_value(cold).unwrap(), serde_json::to_value(first).unwrap());
    let cache = store.smalltalk.subject_cache.lock().unwrap();
    assert_eq!(cache.card_statuses.len(), AGENT_CARD_STATUS_LIMIT);
    assert!(!cache.card_statuses.contains_key("agent/cache/cold"));
    drop(cache);
    store.forget_current_views();
    assert!(store.smalltalk.subject_cache.lock().unwrap().card_statuses.is_empty());
    store.agent_card_status_at(None, cut, true).unwrap();
    assert_eq!(store.smalltalk.subject_cache.lock().unwrap().card_statuses.len(), 1);
}

#[test]
fn agent_card_status_cache_follows_owning_generation_changes() {
    let store = Store::open_memory("node").unwrap();
    publish_mission(&store, r#"version 2
mission "card-owner" state="ready" {
  goal "Fence card status to its owner."; step "work" { agentless }
}"#, "card-owner-mission");
    let run = store.create_mission_run(&MissionRunRequest {
        mission: "card-owner".into(), revision: None, workspace: "/tmp".into(),
        requester: Some("person/test".into()), mode: Some("run".into()),
        inputs: BTreeMap::new(), idempotency_key: "card-owner-run".into(),
    }).unwrap();
    let mut intent = parse_intent("version 2\nagent \"worker\" { command \"true\" }\n", "node").unwrap();
    let desired = intent.subjects.values_mut().next().unwrap();
    desired.owner_run = Some(run.subject.clone());
    desired.owner_generation = Some(run.generation.clone());
    let preview = store.mission(&intent, IntentInput { kdl: String::new(), source_name: None }).unwrap();
    store.apply(&intent, &preview.subject_tokens, "card-owned-runtime").unwrap();
    let first = store.index().unwrap();
    let before = store.agent_card_status_at(None, first, true).unwrap();
    assert_eq!(before.subjects[0].projection.layer, "current");
    store.append_claim(&ClaimInput {
        subject: run.generation, kind: "run-generation.superseded".into(),
        actor: Some("person/test".into()), fields: BTreeMap::from([
            ("status".into(), json!("superseded")),
            ("successor".into(), json!("run-generation/replacement")),
            ("reason".into(), json!("test revision")),
        ]), evidence: Vec::new(), expected_subject: None, idempotency_key: None,
    }).unwrap();
    let index = store.index().unwrap();
    let after = store.agent_card_status_at(None, index, true).unwrap();
    assert_eq!(after.subjects[0].projection.layer, "history");
    assert!(store.agent_card_status_at(None, index, false).unwrap().subjects.is_empty());
    assert_eq!(serde_json::to_value(store.agent_card_status_at(None, first, true).unwrap()).unwrap(),
        serde_json::to_value(before).unwrap());
    store.forget_current_views();
    assert_eq!(serde_json::to_value(store.agent_card_status_at(None, index, true).unwrap()).unwrap(),
        serde_json::to_value(after).unwrap());
}

#[test]
fn status_projection_threads_inherit_cancellation_and_release_their_readers() {
    let store = Store::open_memory("node").unwrap();
    let names = (0..65).map(|n| format!("agent/budget/seat-{n}")).collect::<BTreeSet<_>>();
    let index = store.index().unwrap();
    let expired = smallclaims::read_budget::ReadBudget::new("/parallel-status", std::time::Duration::ZERO);
    smallclaims::read_budget::with(Some(expired), || {
        let error = store.status_for_subject_names_at(names.clone(), index, true, ClaimsReduction::Full).unwrap_err();
        assert_eq!(smallclaims::error::typed(error).code, "read-deadline");
    });
    let eligible = smallclaims::read_budget::ReadBudget::new("/parallel-status", std::time::Duration::from_secs(15));
    let result = smallclaims::read_budget::with(Some(eligible.clone()), || {
        store.status_for_subject_names_at(names, index, true, ClaimsReduction::Full)
    }).unwrap();
    assert_eq!(result.subjects.len(), 65);
    assert!(!eligible.expired());
    assert!(store.readers.get().is_autocommit());
}

#[test]
fn bound_one_nested_large_status_reuses_its_snapshot_reader() {
    let store = Arc::new(smallclaims::sqlite::with_read_limit_for_test(
        1, || Store::open_memory("node").unwrap(),
    ));
    let names = (0..80).map(|n| format!("agent/admitted/seat-{n:03}"))
        .collect::<BTreeSet<_>>();
    for name in &names {
        observe_runtime(&store, name, "running", "incarnation");
    }
    let budget = smallclaims::read_budget::ReadBudget::new(
        "/nested-status", std::time::Duration::from_secs(15),
    );
    let worker_budget = budget.clone();
    let worker_store = store.clone();
    let expected = names.iter().cloned().collect::<Vec<_>>();
    let (done, finished) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let result = smallclaims::read_budget::with(Some(worker_budget), || {
            worker_store.readers.request_read(|| {
                worker_store.read_snapshot(|cut| {
                    worker_store.readers.request_read(|| {
                        worker_store.status_for_subject_names_at(names, cut, true, ClaimsReduction::Full)
                    })?
                })
            })?
        });
        done.send(result).unwrap();
    });
    let completed = finished.recv_timeout(std::time::Duration::from_secs(5));
    if completed.is_err() {
        budget.cancel();
    }
    let result = completed.expect("bound-one nested status fanout deadlocked").unwrap();
    worker.join().unwrap();
    assert_eq!(result.subjects.iter().map(|subject| subject.subject.clone())
        .collect::<Vec<_>>(), expected);
    assert_eq!(store.readers.usage().peak, 1);
    assert_eq!(store.readers.usage().idle, 1);
    assert!(!budget.expired());
}

#[test]
fn admitted_status_fanout_and_inline_reduction_have_identical_values_and_digests() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("status.sqlite3");
    let inline = smallclaims::sqlite::with_read_limit_for_test(
        1, || Store::open(&path, "node").unwrap(),
    );
    let names = (0..80).map(|n| format!("agent/admitted/seat-{n:03}"))
        .collect::<BTreeSet<_>>();
    for name in &names {
        observe_runtime(&inline, name, "stopped", "old");
        observe_runtime(&inline, name, "running", "current");
    }
    // Two fresh pools over the same real claims keep provenance and accepted times
    // identical, making the entire serialized result (not a normalized subset) comparable.
    let parallel = smallclaims::sqlite::with_read_limit_for_test(
        32, || Store::open(&path, "node").unwrap(),
    );
    let cut = inline.index().unwrap();
    assert_eq!(parallel.index().unwrap(), cut);
    for history in [false, true] {
        let values = [&inline, &parallel].map(|store| {
            let budget = smallclaims::read_budget::ReadBudget::new(
                "/admitted-status", std::time::Duration::from_secs(15),
            );
            let result = smallclaims::read_budget::with(Some(budget), || {
                store.readers.request_read(|| {
                    store.status_for_subject_names_at(names.clone(), cut, history, ClaimsReduction::Full)
                })
            }).unwrap().unwrap();
            assert_eq!(result.subjects.len(), names.len());
            assert!(result.subjects.windows(2).all(|pair| pair[0].subject < pair[1].subject));
            serde_json::to_value(result).unwrap()
        });
        assert_eq!(values[0], values[1]);
        let digests = values.map(|value| {
            hex::encode(Sha256::digest(serde_json::to_vec(&value).unwrap()))
        });
        assert_eq!(digests[0], digests[1]);
    }
    assert_eq!(inline.readers.usage().peak, 1);
    assert!(parallel.readers.usage().peak > 1);
    assert!(parallel.readers.usage().peak <= 32);
}

#[test]
fn current_runtime_views_reduce_what_they_show_not_every_runtime_ever_held() {
    let store = Store::open_memory("node").unwrap();
    observe_runtime(&store, "agent/bench/live", "running", "live-1");
    let retire = |from: usize, to: usize| {
        for number in from..to {
            let subject = format!("agent/bench/old-{number:03}");
            observe_runtime(&store, &subject, "running", "old");
            observe_runtime(&store, &subject, "stopped", "old");
        }
    };
    retire(0, 40);
    let live = vec!["agent/bench/live".to_owned()];
    assert_eq!(current_runtimes(&store), (live.clone(), 1));
    // Nothing changed, so nothing is reduced again.
    assert_eq!(current_runtimes(&store), (live.clone(), 0));
    // Three times the history leaves the work where it was.
    retire(40, 120);
    assert_eq!(current_runtimes(&store), (live.clone(), 0));
    // A claim about the live runtime reduces it again, and only it.
    observe_runtime(&store, "agent/bench/live", "running", "live-2");
    assert_eq!(current_runtimes(&store), (live.clone(), 1));
    // The view is the current layer of the full reduction.
    let full = store
        .status_for_claim_kind_at("runtime.observed", None, true, ClaimsReduction::Summary)
        .unwrap();
    assert_eq!(full.subjects.len(), 121);
    let current_layer = full
        .subjects
        .into_iter()
        .filter(|status| status.projection.layer == "current")
        .map(|status| status.subject)
        .collect::<Vec<_>>();
    assert_eq!(current_layer, live);
}

#[test]
fn kept_statuses_match_a_fresh_reduction_as_claims_arrive() {
    let store = Store::open_memory("node").unwrap();
    let mut seed = 0x5eed_u64;
    let mut next = || {
        seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (seed >> 33) as usize
    };
    let statuses = ["running", "stopped", "starting", "exited", "idle"];
    let states = ["idle", "working", "ready"];
    for step in 0..240 {
        let seat = next() % 10;
        let subject = format!("agent/bench/seat-{seat}");
        let incarnation = format!("seat-{seat}-{}", next() % 2);
        if next() % 2 == 0 {
            observe_runtime(
                &store,
                &subject,
                statuses[next() % statuses.len()],
                &incarnation,
            );
        } else {
            store
                .append_claim(&ClaimInput {
                    subject: subject.clone(),
                    kind: "harness.observed".into(),
                    actor: Some(subject.clone()),
                    fields: BTreeMap::from([
                        (
                            "state".into(),
                            Value::String(states[next() % states.len()].into()),
                        ),
                        ("incarnation_id".into(), Value::String(incarnation)),
                        ("driver".into(), Value::String("claude".into())),
                    ]),
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: None,
                })
                .unwrap();
        }
        if step % 6 != 5 {
            continue;
        }
        for history in [false, true] {
            let view = |store: &Store| {
                serde_json::to_value(
                    store
                        .status_for_claim_kind_at(
                            "runtime.observed",
                            None,
                            history,
                            ClaimsReduction::Summary,
                        )
                        .unwrap(),
                )
                .unwrap()
            };
            let kept = view(&store);
            store.forget_current_views();
            assert_eq!(kept, view(&store), "step {step}, history {history}");
        }
    }
}

#[test]
fn a_kept_status_follows_work_its_agent_does_on_other_subjects() {
    let store = Store::open_memory("node").unwrap();
    let agent = "agent/bench/worker";
    observe_runtime(&store, agent, "running", "worker-1");
    store
        .append_claim(&ClaimInput {
            subject: agent.into(),
            kind: "harness.observed".into(),
            actor: Some(agent.into()),
            fields: BTreeMap::from([
                ("state".into(), Value::String("idle".into())),
                ("incarnation_id".into(), Value::String("worker-1".into())),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    let harness_state = |store: &Store| {
        store
            .status_for_claim_kind_at(
                "runtime.observed",
                None,
                false,
                ClaimsReduction::Summary,
            )
            .unwrap()
            .subjects
            .into_iter()
            .find(|status| status.subject == agent)
            .and_then(|status| status.harness)
            .map(|harness| harness.state)
    };
    assert_eq!(harness_state(&store).as_deref(), Some("idle"));
    // Work the agent reports on a step, a claim about another subject, makes it working.
    {
        let mut connection = store.connection.lock().unwrap();
        let transaction = connection.transaction().unwrap();
        append_claim_tx(
            &transaction,
            &store.origin,
            "step-run/bench/work",
            "work.progress",
            Some(agent),
            &json!({"fields": {
                "attempt": 1,
                "status": "working",
                "worker_reported": true,
                "claimant": agent,
                "claim_incarnation": "worker-1",
                "claim_expires_at_unix_ms": now_ms().saturating_add(60_000),
                "readiness_epoch": 1,
            }}),
            &[],
            None,
        )
        .unwrap();
        transaction.commit().unwrap();
    }
    assert_eq!(harness_state(&store).as_deref(), Some("working"));
}

#[test]
fn the_newest_message_to_a_recipient_is_found_by_index() {
    let store = Store::open_memory("node").unwrap();
    let connection = store.readers.get();
    let plan = connection
        .prepare(
            "EXPLAIN QUERY PLAN SELECT accepted_at_unix_ms FROM claims
                 WHERE kind='message.sent' AND json_extract(body, '$.fields.to')=?1
                   AND store_index<=?2 ORDER BY store_index DESC LIMIT 1",
        )
        .unwrap()
        .query_map(params!["agent/bench/reader", 10], |row| {
            row.get::<_, String>(3)
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(
        plan.iter()
            .any(|step| step.contains("claims_message_to_order_index")),
        "{plan:?}"
    );
}

fn query_plan(store: &Store, sql: &str, params: impl rusqlite::Params) -> Vec<String> {
    store
        .readers
        .get()
        .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
        .unwrap()
        .query_map(params, |row| row.get::<_, String>(3))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

fn listed(store: &Store, sql: &str, params: impl rusqlite::Params) -> Vec<String> {
    store
        .readers
        .get()
        .prepare(sql)
        .unwrap()
        .query_map(params, |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

/// Claims written as bare rows, about subjects no claim spec admits, and the store index of
/// the last one.
fn insert_raw_claims(store: &Store, claims: &[(&str, &str)]) -> u64 {
    let connection = store.connection.lock().unwrap();
    connection
        .execute_batch("PRAGMA foreign_keys = OFF")
        .unwrap();
    for (subject, kind) in claims {
        connection
            .execute(
                "INSERT INTO claims(id, batch_id, subject, kind, origin, actor, body,
                                        predecessors, accepted_at_unix_ms)
                     VALUES (?1, 'batch/raw', ?2, ?3, 'node', NULL, '{}', '[]', '1')",
                params![Uuid::now_v7().to_string(), subject, kind],
            )
            .unwrap();
    }
    connection
        .execute_batch("PRAGMA foreign_keys = ON")
        .unwrap();
    connection
        .query_row("SELECT MAX(store_index) FROM claims", [], |row| row.get(0))
        .unwrap()
}

/// The replication projection's two frontier checks seek an index instead of reading every
/// claim, and answer as the scans they replace at every snapshot.
#[test]
fn replication_frontier_checks_seek_instead_of_scanning() {
    let store = Store::open_memory("node").unwrap();
    let mut snapshots = vec![0];
    {
        let connection = store.connection.lock().unwrap();
        connection
            .execute_batch("PRAGMA foreign_keys = OFF")
            .unwrap();
        for (subject, batch, accepted) in [
            ("mission-run/one", "batch/left/1/a", "9"),
            ("run-generation/gen", "batch/left/2/a", "10"),
            ("step-run/gen/build", "batch/right/1/a", "10"),
            // Just outside the steps of generation `gen`.
            ("step-run/gen0", "batch/right/2/a", "100"),
            ("step-run/ge/test", "batch/left/3/a", "11"),
        ] {
            connection
                .execute(
                    "INSERT INTO claims(id, batch_id, subject, kind, origin, actor, body,
                                            predecessors, accepted_at_unix_ms)
                         VALUES (?1, ?2, ?3, 'harness.observed', 'node', NULL, '{}', '[]', ?4)",
                    params![Uuid::now_v7().to_string(), batch, subject, accepted],
                )
                .unwrap();
            snapshots.push(
                connection
                    .query_row("SELECT MAX(store_index) FROM claims", [], |row| row.get(0))
                    .unwrap(),
            );
        }
        connection
            .execute_batch("PRAGMA foreign_keys = ON")
            .unwrap();
    }
    let batches_scan = "SELECT batch_id FROM claims
                            WHERE store_index<=?1 AND accepted_at_unix_ms=?2";
    let prior_scan = "SELECT EXISTS(SELECT 1 FROM claims WHERE store_index<=?1
                          AND (subject=?2 OR subject=?3 OR subject LIKE ?4))";
    let exists = |sql: &str, params: &[&dyn rusqlite::ToSql]| -> bool {
        store
            .readers
            .get()
            .query_row(sql, params, |row| row.get(0))
            .unwrap()
    };
    for &snapshot in &snapshots {
        for accepted in ["1", "9", "10", "11", "100"] {
            let mut sought = listed(&store, LAST_ACCEPTED_BATCHES, params![snapshot, accepted]);
            let mut scanned = listed(&store, batches_scan, params![snapshot, accepted]);
            sought.sort();
            scanned.sort();
            assert_eq!(sought, scanned, "accepted {accepted} at {snapshot}");
        }
        for (run, generation, id) in [
            ("mission-run/one", "run-generation/none", "none"),
            ("mission-run/none", "run-generation/gen", "gen"),
            ("mission-run/none", "run-generation/other", "gen"),
            ("mission-run/none", "run-generation/other", "ge"),
            ("mission-run/none", "run-generation/other", "other"),
        ] {
            let (from, to) = (format!("step-run/{id}/"), format!("step-run/{id}0"));
            let pattern = format!("step-run/{id}/%");
            assert_eq!(
                exists(
                    RUN_HAS_PRIOR_CLAIMS,
                    params![snapshot, run, generation, from, to]
                ),
                exists(prior_scan, params![snapshot, run, generation, pattern]),
                "{run} {generation} {id} at {snapshot}"
            );
        }
    }
    let plan = query_plan(&store, LAST_ACCEPTED_BATCHES, params![10, "10"]);
    assert!(
        plan.iter()
            .any(|step| step.contains("claims_accepted_order_index")),
        "{plan:?}"
    );
    let plan = query_plan(
        &store,
        RUN_HAS_PRIOR_CLAIMS,
        params![
            10,
            "mission-run/one",
            "run-generation/gen",
            "step-run/gen/",
            "step-run/gen0"
        ],
    );
    let reads = plan
        .iter()
        .filter(|step| step.contains("claims"))
        .collect::<Vec<_>>();
    assert_eq!(reads.len(), 3, "{plan:?}");
    assert!(
        reads
            .iter()
            .all(|step| step.contains("INDEX claims_subject_index (subject")),
        "{plan:?}"
    );
}

/// The runtimes' subjects are found one index seek per subject, not by reading every runtime
/// observation, and they are the subjects a scan finds at every snapshot.
#[test]
fn runtime_subjects_are_read_one_seek_per_subject() {
    let store = Store::open_memory("node").unwrap();
    let mut snapshots = Vec::new();
    for round in 0..4 {
        let subjects = (0..=round)
            .map(|number| format!("agent/kind/runtime-{number}"))
            .collect::<Vec<_>>();
        let mut claims = subjects
            .iter()
            .flat_map(|subject| {
                [
                    (subject.as_str(), "runtime.observed"),
                    (subject.as_str(), "harness.observed"),
                ]
            })
            .collect::<Vec<_>>();
        let other = format!("agent/kind/other-{round}");
        claims.push((&other, "harness.observed"));
        snapshots.push(insert_raw_claims(&store, &claims));
    }
    let scan = "SELECT DISTINCT subject FROM claims WHERE kind='runtime.observed'
                    AND store_index<=?1 ORDER BY subject";
    for snapshot in [0, 1, snapshots[0], snapshots[1], snapshots[3]] {
        assert_eq!(
            listed(&store, RUNTIME_SUBJECTS, [snapshot]),
            listed(&store, scan, [snapshot]),
            "at {snapshot}"
        );
    }
    let plan = query_plan(&store, RUNTIME_SUBJECTS, [10]);
    assert!(
        plan.iter()
            .any(|step| step.contains("claims_runtime_subject_index (subject>?)")),
        "{plan:?}"
    );
    assert!(
        plan.iter().all(|step| !step.contains("TEMP B-TREE")),
        "{plan:?}"
    );
}

/// A prefix's subjects are found one index seek per subject, and they are the subjects a
/// GLOB scan finds, including the neighbours just outside the prefix's range.
#[test]
fn a_prefixs_subjects_are_read_one_seek_per_subject() {
    assert_eq!(prefix_upper_bound("agent/").as_deref(), Some("agent0"));
    assert_eq!(prefix_upper_bound("host/").as_deref(), Some("host0"));
    for unbounded in ["", "agent*", "a?", "[a]/", "agent/\u{e9}", "agent\u{7f}"] {
        assert_eq!(prefix_upper_bound(unbounded), None, "{unbounded:?}");
    }
    let store = Store::open_memory("node").unwrap();
    let first = insert_raw_claims(
        &store,
        &[
            ("agen/x", "harness.observed"),
            ("agent", "harness.observed"),
            ("agent/", "harness.observed"),
            ("agent/a", "harness.observed"),
            ("agent/a", "runtime.observed"),
            ("agent/a/b", "harness.observed"),
            ("agent0", "harness.observed"),
            ("agents/x", "harness.observed"),
            ("Agent/x", "harness.observed"),
            ("host/one", "transport.observed"),
        ],
    );
    let last = insert_raw_claims(
        &store,
        &[
            ("agent/a", "harness.observed"),
            ("agent/late", "harness.observed"),
            ("agent/z", "harness.observed"),
            ("host/two", "transport.observed"),
        ],
    );
    let scan = "SELECT DISTINCT subject FROM claims WHERE store_index<=?1 AND subject GLOB ?2
                    ORDER BY subject";
    for snapshot in [0, 1, 4, first, last] {
        for prefix in ["agent/", "host/", "agent/a/"] {
            let bound = prefix_upper_bound(prefix).unwrap();
            assert_eq!(
                listed(&store, RANGE_SUBJECTS, params![snapshot, prefix, bound]),
                listed(&store, scan, params![snapshot, format!("{prefix}*")]),
                "{prefix} at {snapshot}"
            );
        }
    }
    let plan = query_plan(&store, RANGE_SUBJECTS, params![last, "agent/", "agent0"]);
    assert!(
        plan.iter()
            .any(|step| step.contains("claims_subject_index (subject>? AND subject<?)")),
        "{plan:?}"
    );
    assert!(
        plan.iter().all(|step| !step.contains("TEMP B-TREE")),
        "{plan:?}"
    );
}

/// A seat's held and ready steps are read from the fleet's open steps, not from every step the
/// store has run.
#[test]
fn seat_steps_are_read_from_the_open_steps() {
    let store = Store::open_memory("node").unwrap();
    for agent in [None, Some("agent/seat")] {
        let steps = query_plan(&store, SEAT_STEP_ROWS, params![agent, now_ms().to_string()]);
        assert!(
            steps
                .iter()
                .any(|step| step.contains("step_runs_open_index")),
            "{steps:?}"
        );
    }
}

/// The machine list's held steps are the client work list's and work history's steps that
/// a claimant holds, read from the leased steps alone.
#[test]
fn held_work_is_read_from_the_leased_steps() {
    let store = Store::open_memory("node").unwrap();
    let source = r#"
version 2

  mission "held-work" state="ready" {
    goal "Hold some steps."
    concurrent-runs max=2
    step "active" { assigned-to "agent/worker" }
    step "expired" { assigned-to "agent/worker" }
    step "ready" { assigned-to "agent/worker" }
    step "pending" { assigned-to "agent/worker" }
    step "watch" { agentless }
  }

"#;
    let intent = crate::graph::parse_test_intent(source, "node").unwrap();
    let planned = store
        .mission(
            &intent,
            IntentInput {
                kdl: source.into(),
                source_name: None,
            },
        )
        .unwrap();
    store
        .apply(&intent, &planned.subject_tokens, "held-work-mission")
        .unwrap();
    let mut runs = Vec::new();
    for number in 0..2 {
        runs.push(
            store
                .create_mission_run(&MissionRunRequest {
                    mission: "held-work".into(),
                    revision: None,
                    workspace: "/tmp".into(),
                    requester: Some("person/test".into()),
                    mode: Some("run".into()),
                    inputs: BTreeMap::new(),
                    idempotency_key: format!("held-work-run-{number}"),
                })
                .unwrap(),
        );
    }
    let request = |key: &str| WorkRequest {
        wait_ms: None,
        actor: Some("agent/node.worker".into()),
        incarnation: Some("worker-one".into()),
        summary: None,
        reason: None,
        evidence: Vec::new(),
        idempotency_key: key.into(),
    };
    // The second run holds a step until it ends; then the first run holds one.
    for (number, run) in runs.iter().enumerate().rev() {
        let subjects = run
            .steps
            .iter()
            .map(|step| (step.step.clone(), step.subject.clone()))
            .collect::<BTreeMap<_, _>>();
        for path in ["active", "expired", "ready"] {
            store
                .set_step_state(&subjects[path], "ready", None)
                .unwrap();
        }
        for path in ["expired", "active"] {
            store
                .work_action(
                    &subjects[path],
                    "claim",
                    &request(&format!("{path}-{number}")),
                )
                .unwrap();
            if path == "expired" {
                expire_work_lease_by_claim(
                    &store,
                    &subjects[path],
                    "agent/node.worker",
                    "worker-one",
                );
            }
        }
        if number == 1 {
            assert!(
                store
                    .set_mission_run_state(&run.id, "cancelled", "terminal", Some("stopped"))
                    .unwrap()
            );
        }
    }
    let held = |views: Vec<StepRunView>| {
        views
            .into_iter()
            .filter(|view| view.claimant.is_some())
            .map(|view| (view.subject, view.claimant, view.updated_at_unix_ms))
            .collect::<BTreeSet<_>>()
    };
    let now = now_ms();
    // Later, the active lease has expired too.
    for at in [now, now + 86_400_000] {
        let current = held(store.client_work_at_snapshot(None, false, at).unwrap());
        let history = held(store.client_work_history_at_snapshot(None, at).unwrap());
        assert_eq!(
            held(store.client_work_claims_at_snapshot(false, at).unwrap()),
            current
        );
        assert_eq!(
            held(store.client_work_claims_at_snapshot(true, at).unwrap()),
            history
        );
        assert_eq!(current.len(), usize::from(at == now), "{current:?}");
    }
    STEPS_ENRICHED.with(|enriched| enriched.set(0));
    store.client_work_claims_at_snapshot(true, now).unwrap();
    assert_eq!(STEPS_ENRICHED.with(std::cell::Cell::get), 0);
}

/// Each listed agent's member fault is its newest member-reconcile decision, read per agent.
#[test]
fn member_faults_are_read_for_the_listed_agents() {
    let store = Store::open_memory("node").unwrap();
    let decide = |subject: &str, decision: &str, reason: &str| {
        store
            .append_claim(&ClaimInput {
                subject: subject.into(),
                kind: "runtime.reconcile-decision".into(),
                actor: None,
                fields: BTreeMap::from([
                    ("key".into(), Value::String("member-reconcile".into())),
                    ("decision".into(), Value::String(decision.into())),
                    ("reason".into(), Value::String(reason.into())),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap()
    };
    decide("agent/faults/healed", "member-fault", "it failed once");
    let failed = decide("agent/faults/failed", "member-fault", "it cannot start");
    decide("agent/faults/healed", "member-started", "it started");
    decide("agent/faults/failed", "member-started", "it started");
    let subjects = [
        "agent/faults/healed",
        "agent/faults/failed",
        "agent/faults/none",
    ]
    .map(str::to_owned)
    .to_vec();
    let index = store.index().unwrap();
    for at in [failed.store_index, index] {
        let expected = subjects
            .iter()
            .filter_map(|subject| {
                store
                    .member_reconcile_fault(subject, Some(at))
                    .unwrap()
                    .map(|fault| (subject.clone(), fault))
            })
            .collect::<BTreeMap<_, _>>();
        assert_eq!(
            store.member_reconcile_faults_for(&subjects, at).unwrap(),
            expected
        );
    }
    assert_eq!(
        store
            .member_reconcile_faults_for(&subjects, failed.store_index)
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        ["agent/faults/failed", "agent/faults/healed"]
    );
}

#[test]
fn the_actual_cache_follows_each_subjects_newest_claim() {
    let store = Store::open_memory("node").unwrap();
    let observe = |subject: &str, status: &str| {
        store
            .append_claim(&ClaimInput {
                subject: subject.into(),
                kind: "runtime.observed".into(),
                actor: None,
                fields: BTreeMap::from([("status".into(), Value::String(status.into()))]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap()
    };
    let subject = "agent/run/worker";
    let stopped = observe(subject, "stopped");
    assert_eq!(
        store.latest_actual_value(subject).unwrap().unwrap()["status"],
        "stopped"
    );
    let cached = |store: &Store| {
        store
            .smalltalk
            .actual_cache
            .lock()
            .unwrap()
            .get(subject)
            .cloned()
    };
    assert_eq!(cached(&store).unwrap().0, stopped.store_index);

    // Other subjects' writes leave a stopped runtime's folded state valid.
    observe("agent/run/other", "running");
    let before = cached(&store).unwrap();
    store.latest_actual_value(subject).unwrap();
    assert_eq!(
        cached(&store).unwrap(),
        before,
        "an unrelated write must not refold this subject's history"
    );

    let running = observe(subject, "running");
    assert_eq!(
        store.latest_actual_value(subject).unwrap().unwrap()["status"],
        "running"
    );
    assert_eq!(cached(&store).unwrap().0, running.store_index);
}

#[test]
fn a_subject_with_no_actual_or_desired_state_has_unknown_reachability() {
    let store = Store::open_memory("node").unwrap();
    let subject = "agent/diagnostic-run/sig.hub";
    store
        .append_claim(&ClaimInput {
            subject: subject.into(),
            kind: "harness.observed".into(),
            actor: Some(subject.into()),
            fields: BTreeMap::from([("state".into(), Value::String("idle".into()))]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    let status = store.status(Some(subject)).unwrap();
    assert!(status.subjects[0].actual.is_none());
    assert!(status.subjects[0].desired.is_none());
    assert_eq!(status.subjects[0].reachability, "unknown");
}

#[test]
fn a_same_origin_restart_window_does_not_create_runtime_conflict() {
    let store = Store::open_memory("node").unwrap();
    let subject = "agent/run/worker";
    let observe = |status: &str, incarnation: &str| {
        store
            .append_claim(&ClaimInput {
                subject: subject.into(),
                kind: "runtime.observed".into(),
                actor: None,
                fields: BTreeMap::from([
                    ("status".into(), Value::String(status.into())),
                    ("incarnation_id".into(), Value::String(incarnation.into())),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
    };
    observe("running", "one");
    store
        .append_claim(&ClaimInput {
            subject: subject.into(),
            kind: "runtime.restart-window-reset".into(),
            actor: None,
            fields: BTreeMap::from([
                ("desired_token".into(), Value::String("desired-one".into())),
                ("incarnation_id".into(), Value::String("one".into())),
                (
                    "reason".into(),
                    Value::String("the stable interval elapsed".into()),
                ),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    observe("running", "two");

    let status = store.status(Some(subject)).unwrap();
    assert_eq!(status.subjects[0].reachability, "reachable");
    assert_ne!(
        status.subjects[0].reason.as_deref(),
        Some("concurrent runtime observations have indeterminate authority")
    );
}

#[test]
fn concurrent_cross_origin_runtime_observations_are_indeterminate() {
    let left = Store::open_memory("left").unwrap();
    let right = Store::open_memory("right").unwrap();
    for (store, incarnation) in [(&left, "left-one"), (&right, "right-one")] {
        store
            .append_claim(&ClaimInput {
                subject: "agent/run/worker".into(),
                kind: "runtime.observed".into(),
                actor: None,
                fields: BTreeMap::from([
                    ("status".into(), Value::String("running".into())),
                    ("incarnation_id".into(), Value::String(incarnation.into())),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
    }
    left.import_replication("right", &right.export_replication(0).unwrap())
        .unwrap();

    let status = left.status(Some("agent/run/worker")).unwrap();
    assert_eq!(status.subjects[0].reachability, "indeterminate");
    assert_eq!(
        status.subjects[0].reason.as_deref(),
        Some("concurrent runtime observations have indeterminate authority")
    );
}

#[test]
fn a_rival_origins_latest_observation_determines_runtime_authority() {
    let subject = "agent/run/worker";
    for terminal_status in ["stopped", "absent", "exited", "vanished"] {
        let left = Store::open_memory("left").unwrap();
        let right = Store::open_memory("right").unwrap();
        left.set_write_clock_at(1_800_000_000_000).unwrap();
        right.set_write_clock_at(1_800_000_000_001).unwrap();
        let observe = |store: &Store, status: &str, host: &str| {
            store
                .append_claim(&ClaimInput {
                    subject: subject.into(),
                    kind: "runtime.observed".into(),
                    actor: None,
                    fields: BTreeMap::from([
                        ("status".into(), json!(status)),
                        ("host".into(), json!(host)),
                        ("incarnation_id".into(), json!(host)),
                    ]),
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: None,
                })
                .unwrap()
        };
        observe(&left, "running", "left");
        observe(&left, terminal_status, "left");
        // The new owner has no causal path through the old owner's runtime branch.
        let selected = observe(&right, "running", "right");
        right.import_replication("left", &left.export_replication(0).unwrap())
            .unwrap();
        let view = right.status(Some(subject)).unwrap().subjects.remove(0);
        assert_eq!(view.actual_claim.as_deref(), Some(selected.id.as_str()));
        assert_eq!(view.reachability, "reachable", "{terminal_status}: {view:?}");
        // A later live rival is not hidden by its previous terminal observation.
        left.set_write_clock_at(1_800_000_000_002).unwrap();
        observe(&left, "running", "left");
        right.import_replication("left", &left.export_replication(0).unwrap())
            .unwrap();
        assert_eq!(
            right.status(Some(subject)).unwrap().subjects[0].reachability,
            "indeterminate"
        );
    }
}

/// A subject's harness reports neither change which observation its status selects nor hide
/// the causal path between two hosts' observations, although the reads that select skip them.
#[test]
fn harness_reports_neither_select_nor_hide_a_runtime_observation() {
    let subject = "agent/run/worker";
    let append = |store: &Store, kind: &str, fields: &[(&str, &str)]| {
        store
            .append_claim(&ClaimInput {
                subject: subject.into(),
                kind: kind.into(),
                actor: Some(subject.into()),
                fields: fields
                    .iter()
                    .map(|(name, value)| ((*name).into(), Value::String((*value).into())))
                    .collect(),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap()
    };
    let report = |store: &Store, count: usize| {
        for _ in 0..count {
            append(store, "harness.observed", &[("state", "working")]);
        }
    };
    let observe = |store: &Store, incarnation: &str| {
        append(
            store,
            "runtime.observed",
            &[("status", "running"), ("incarnation_id", incarnation)],
        )
    };

    // The left host observes the runtime after it has taken in the right host's
    // observation, with harness reports between the two: no conflict.
    let left = Store::open_memory("left").unwrap();
    let right = Store::open_memory("right").unwrap();
    observe(&right, "right-one");
    report(&right, 3);
    left.import_replication("right", &right.export_replication(0).unwrap())
        .unwrap();
    report(&left, 3);
    let selected = observe(&left, "left-two");
    report(&left, 3);
    let status = left.status(Some(subject)).unwrap();
    assert_eq!(status.subjects[0].reachability, "reachable");
    assert_eq!(
        status.subjects[0].actual_claim.as_deref(),
        Some(selected.id.as_str())
    );
    assert_eq!(
        status.subjects[0].actual.as_ref().unwrap()["incarnation_id"],
        "left-two"
    );

    // Two hosts observe it without seeing each other, harness reports on both sides.
    let other = Store::open_memory("other").unwrap();
    observe(&other, "other-one");
    report(&other, 3);
    left.import_replication("other", &other.export_replication(0).unwrap())
        .unwrap();
    let status = left.status(Some(subject)).unwrap();
    assert_eq!(status.subjects[0].reachability, "indeterminate");

    // The selecting reads seek past the harness reports instead of reading them.
    let connection = left.readers.get();
    let plan = connection
        .prepare(&format!(
            "EXPLAIN QUERY PLAN SELECT claims.id FROM claims
                 INDEXED BY claims_subject_kind_index
                 JOIN batches ON batches.id=claims.batch_id
                 WHERE claims.subject=?1 AND {ACTUAL_STATE_CLAIM} AND claims.store_index<=?2
                 ORDER BY {CANONICAL_ORDER}"
        ))
        .unwrap()
        .query_map(params![subject, i64::MAX], |row| row.get::<_, String>(3))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(
        plan.iter()
            .any(|step| step.contains("claims_subject_kind_index (subject=? AND kind<?)")),
        "{plan:?}"
    );
    assert!(
        plan.iter()
            .any(|step| step.contains("claims_subject_kind_index (subject=? AND kind>?)")),
        "{plan:?}"
    );
}

#[test]
fn a_remote_stop_without_process_authority_cannot_poison_the_running_owner() {
    let owner = json!({"fields": {"status": "running", "host": "ExampleMac"}});
    let remote_stop = json!({"fields": {"status": "stopped"}});
    let remote_running = json!({"fields": {"status": "running", "host": "example-linux"}});
    assert!(nonowner_terminal_observation(
        None,
        "ExampleMac",
        &owner,
        "example-linux",
        &remote_stop,
    ));
    assert!(!nonowner_terminal_observation(
        Some("example-linux"),
        "ExampleMac",
        &owner,
        "example-linux",
        &remote_stop,
    ));
    assert!(!nonowner_terminal_observation(
        None,
        "ExampleMac",
        &owner,
        "example-linux",
        &remote_running,
    ));
}

#[test]
fn work_views_include_ordered_inherited_constraints() {
    let store = Store::open_memory("node").unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let mission = publish_mission(
        &store,
        r#"version 2
mission "guarded" state="ready" {
  goal "Complete guarded work."
  input "lane" kind="text"
  constraint "Use the ${input.lane} lane."
  step "parent" {
    agentless
    constraint "Do not push."
    mission "child" {
      goal "Inspect the child workspace."
      constraint "Keep the workspace clean."
      step "inspect" {
        constraint "Do not edit files."
      }
    }
  }
}
"#,
        "publish-guarded",
    );
    let run = store
        .create_mission_run(&MissionRunRequest {
            mission: mission.id,
            revision: Some(mission.revision),
            workspace: workspace.path().display().to_string(),
            requester: Some("person/operator".into()),
            mode: None,
            inputs: BTreeMap::from([("lane".into(), "review".into())]),
            idempotency_key: "run-guarded".into(),
        })
        .unwrap();
    let parent = run.steps.iter().find(|step| step.step == "parent").unwrap();
    assert_eq!(parent.constraints, ["Use the review lane.", "Do not push."]);
    let child = run
        .steps
        .iter()
        .find(|step| step.step.ends_with("child/inspect"))
        .unwrap();
    assert_eq!(
        child.constraints,
        [
            "Use the review lane.",
            "Do not push.",
            "Keep the workspace clean.",
            "Do not edit files."
        ]
    );
}

#[test]
fn agent_block_constraints_bind_only_steps_that_select_the_agent() {
    let store = Store::open_memory("node").unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let source = r#"version 2
mission "handoff" state="ready" {
  goal "Build and merge one change."
  constraint "Keep the change small."
  agent "builder" {
    workspace "."
    command "true"
    constraint "Never push the release branch."
    constraint "Keep the change small."
  }
  agent "merger" { workspace "."; command "true" }
  agent "idle" {
    workspace "."
    command "true"
    constraint "Rest."
  }
  step "build" {
    assigned-to "agent/${ST_MISSION_RUN}/builder"
    constraint "Add a test."
    mission "follow-up" {
      goal "Check the build."
      step "check" {}
    }
  }
  step "either" {
    available-to "agent/${ST_MISSION_RUN}/merger"
    available-to "agent/${ST_MISSION_RUN}/builder"
  }
  step "merge" {
    assigned-to "agent/${ST_MISSION_RUN}/merger"
    depends-on { step "build" completed }
  }
  step "record" { agentless }
  agent "checker" {
    workspace "."
    command "true"
    constraint "Report without editing files."
  }
  step "audit" {
    mission "inspection" {
      goal "Inspect the build."
      assigned-to "agent/${ST_MISSION_RUN}/checker"
      step "look" {}
    }
  }
}
"#;
    let intent = crate::graph::parse_intent(source, "node").unwrap();
    let planned = store
        .mission(
            &intent,
            IntentInput {
                kdl: source.into(),
                source_name: None,
            },
        )
        .unwrap();
    assert!(planned.warnings.contains(
        &"mission `mission/handoff` gives agent `agent/${ST_MISSION_RUN}/idle` constraints, but no step selects that agent"
            .to_owned()
    ));
    assert!(
        !planned
            .warnings
            .iter()
            .any(|warning| warning.contains("checker")),
        "{:?}",
        planned.warnings
    );
    store
        .apply(&intent, &planned.subject_tokens, "publish-handoff")
        .unwrap();
    let mission = intent.missions["handoff"].clone();
    let run = store
        .create_mission_run(&MissionRunRequest {
            mission: mission.id,
            revision: Some(mission.revision),
            workspace: workspace.path().display().to_string(),
            requester: Some("person/operator".into()),
            mode: None,
            inputs: BTreeMap::new(),
            idempotency_key: "run-handoff".into(),
        })
        .unwrap();
    let constraints = |path: &str| {
        run.steps
            .iter()
            .find(|step| step.step == path)
            .unwrap()
            .constraints
            .clone()
    };
    assert_eq!(
        constraints("build"),
        [
            "Keep the change small.",
            "Add a test.",
            "Never push the release branch."
        ]
    );
    assert_eq!(
        constraints("build/follow-up/check"),
        [
            "Keep the change small.",
            "Add a test.",
            "Never push the release branch."
        ]
    );
    assert_eq!(
        constraints("either"),
        ["Keep the change small.", "Never push the release branch."]
    );
    assert_eq!(constraints("merge"), ["Keep the change small."]);
    assert_eq!(constraints("record"), ["Keep the change small."]);
    assert_eq!(constraints("audit"), ["Keep the change small."]);
    assert_eq!(
        constraints("audit/inspection/look"),
        ["Keep the change small.", "Report without editing files."]
    );
}

#[test]
fn apply_is_subject_cas_and_idempotent() {
    let store = Store::open_memory("node").expect("store");
    let intent = simple("true");
    let mission = store
        .mission(
            &intent,
            IntentInput {
                kdl: "test".into(),
                source_name: None,
            },
        )
        .expect("mission");
    let first = store
        .apply(&intent, &mission.subject_tokens, "one")
        .expect("apply");
    let repeated = store
        .apply(&intent, &mission.subject_tokens, "one")
        .expect("repeat");
    assert_eq!(first.batch_id, repeated.batch_id);
    let unchanged_mission = store
        .mission(
            &intent,
            IntentInput {
                kdl: "test".into(),
                source_name: None,
            },
        )
        .unwrap();
    let unchanged = store
        .apply(&intent, &unchanged_mission.subject_tokens, "unchanged")
        .unwrap();
    assert!(!unchanged.changed);
    assert!(unchanged.batch_id.is_none());
    assert_eq!(unchanged.store_index, first.store_index);

    let changed = simple("false");
    let error = store
        .apply(&changed, &mission.subject_tokens, "two")
        .expect_err("stale token");
    assert_eq!(error.code, "stale-subject");
    assert_eq!(error.details["subject"], "exec/work");
    assert_eq!(error.details["expected_heads"], json!([]));
    assert_eq!(error.details["current_heads"].as_array().unwrap().len(), 1);
}

#[test]
fn an_internal_apply_replays_after_desired_state_drifts() {
    let store = Store::open_memory("node").unwrap();
    let intent = simple("true");
    let first = store.apply_internal(&intent, "materialize:test").unwrap();
    let replay = store.apply_internal(&intent, "materialize:test").unwrap();

    assert!(first.changed);
    assert!(!replay.changed);
    assert_eq!(replay.store_index, first.store_index);
    assert!(replay.batch_id.is_none());

    let stop =
        crate::graph::parse_test_intent("version 2\nstop \"exec/work\"\n", "node").unwrap();
    assert!(store.apply_internal(&stop, "stop:test").unwrap().changed);

    let restored = store.apply_internal(&intent, "materialize:test").unwrap();
    assert!(restored.changed);
    assert_eq!(
        store.selected_desired_kind("exec/work").unwrap(),
        Some("exec".into())
    );
}

#[test]
fn an_internal_apply_restores_a_repaired_desired_claim() {
    let store = Store::open_memory("node").unwrap();
    let intent = simple("true");
    let first = store.apply_internal(&intent, "materialize:test").unwrap();
    let claim = first.claim_ids.first().expect("desired claim").clone();

    {
        let connection = store.connection.lock().expect("store mutex");
        connection
            .execute(
                "INSERT INTO replica_records(
                         record_ref, writer, sequence, envelope_hash, position, raw, state,
                         claim_id, subject_hint, kind_hint, updated_at_unix_ms
                     ) VALUES (?1, 'node', 1, 'test-envelope', 0, X'', 'repaired',
                               ?2, 'exec/work', 'intent.desired', '0')",
                params![format!("record/{claim}"), claim],
            )
            .unwrap();
        connection
            .execute("DELETE FROM desired WHERE subject='exec/work'", [])
            .unwrap();
    }

    let restored = store.apply_internal(&intent, "materialize:test").unwrap();
    assert!(restored.changed);
    assert_ne!(restored.batch_id, first.batch_id);
    assert_eq!(
        store.selected_desired_kind("exec/work").unwrap(),
        Some("exec".into())
    );
}

#[test]
fn direct_publication_is_additive_atomic_and_operation_ids_are_immutable() {
    let store = Store::open_memory("node").unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let mission = publish_mission(
        &store,
        "version 2\nmission \"lifecycle\" state=\"ready\" { goal \"Remain open.\" }",
        "publish-lifecycle",
    );
    let creation = format!(
        "version 2\nmission-run \"lifecycle/demo\" {{\n  mission {:?}\n  workspace {:?}\n  requester \"person/operator\"\n}}\n",
        format!("mission/lifecycle@{}", mission.revision),
        workspace.path().display().to_string(),
    );
    let intent = crate::graph::parse_intent(&creation, "node").unwrap();
    let preview = store
        .mission(
            &intent,
            IntentInput {
                kdl: creation.clone(),
                source_name: None,
            },
        )
        .unwrap();
    let created = store
        .apply_as(
            &intent,
            &preview.subject_tokens,
            "create-lifecycle-run",
            Some("person/operator"),
        )
        .unwrap();
    assert!(created.changed);
    assert!(
        created
            .operations
            .iter()
            .any(|operation| operation.action == "start-mission-run")
    );

    let replay_preview = store
        .mission(
            &intent,
            IntentInput {
                kdl: creation,
                source_name: None,
            },
        )
        .unwrap();
    let replay = store
        .apply_as(
            &intent,
            &replay_preview.subject_tokens,
            "replay-lifecycle-run",
            Some("person/operator"),
        )
        .unwrap();
    assert!(!replay.changed);

    let omitted_source =
        "version 2\nmission \"unrelated\" state=\"ready\" { goal \"Do other work.\" }\n";
    let omitted = crate::graph::parse_intent(omitted_source, "node").unwrap();
    let omitted_preview = store
        .mission(
            &omitted,
            IntentInput {
                kdl: omitted_source.into(),
                source_name: None,
            },
        )
        .unwrap();
    let omission = store
        .apply_as(
            &omitted,
            &omitted_preview.subject_tokens,
            "omit-existing-run",
            Some("person/operator"),
        )
        .unwrap();
    assert!(omission.changed);
    assert!(store.mission_run("lifecycle/demo").unwrap().is_some());

    let atomic_failure = r#"version 2
mission-run "lifecycle/demo" {
  cancellation "stop" { reason "stop the run" }
}
resource "missing" {
  refresh "now" { timeout "1s" }
}
"#;
    let atomic_intent = crate::graph::parse_intent(atomic_failure, "node").unwrap();
    let atomic_preview = store
        .mission(
            &atomic_intent,
            IntentInput {
                kdl: atomic_failure.into(),
                source_name: None,
            },
        )
        .unwrap();
    assert!(!atomic_preview.blockers.is_empty());
    let error = store
        .apply_as(
            &atomic_intent,
            &atomic_preview.subject_tokens,
            "atomic-failure",
            Some("person/operator"),
        )
        .unwrap_err();
    assert_eq!(error.code, "resource-not-observed");
    assert_eq!(
        store.mission_run("lifecycle/demo").unwrap().unwrap().phase,
        "normal"
    );

    let cancellation = r#"version 2
mission-run "lifecycle/demo" {
  cancellation "stop" { reason "stop the run" }
}
"#;
    let cancel_intent = crate::graph::parse_intent(cancellation, "node").unwrap();
    let cancel_preview = store
        .mission(
            &cancel_intent,
            IntentInput {
                kdl: cancellation.into(),
                source_name: None,
            },
        )
        .unwrap();
    let cancelled = store
        .apply_as(
            &cancel_intent,
            &cancel_preview.subject_tokens,
            "cancel-lifecycle-run",
            Some("person/operator"),
        )
        .unwrap();
    assert!(cancelled.changed);
    let replay_preview = store
        .mission(
            &cancel_intent,
            IntentInput {
                kdl: cancellation.into(),
                source_name: None,
            },
        )
        .unwrap();
    let replay = store
        .apply_as(
            &cancel_intent,
            &replay_preview.subject_tokens,
            "replay-cancellation",
            Some("person/operator"),
        )
        .unwrap();
    assert!(!replay.changed);

    let changed_id = r#"version 2
mission-run "lifecycle/demo" {
  cancellation "stop" { reason "a different reason" }
}
"#;
    let changed_intent = crate::graph::parse_intent(changed_id, "node").unwrap();
    let changed_preview = store
        .mission(
            &changed_intent,
            IntentInput {
                kdl: changed_id.into(),
                source_name: None,
            },
        )
        .unwrap();
    assert!(
        changed_preview
            .blockers
            .iter()
            .any(|blocker| blocker.contains("different content"))
    );
}

#[test]
fn a_declared_planning_session_creates_its_session_and_planner_atomically() {
    let store = Store::open_memory("node").unwrap();
    let request = store
        .put_document(
            "doc/planning/release/request",
            b"Mission the release.",
            &None,
            "planning-request",
        )
        .unwrap();
    let source = format!(
        r#"version 2
planning-session "planning/release/one" {{
  mission "release"
  request "{}@{}"
  workspace "/work/release"
  requester "person/operator"
  planner "codex" {{ model "gpt-5.6-sol"; effort "medium" }}
}}
"#,
        request.name, request.hash
    );
    let intent = crate::graph::parse_intent(&source, "node").unwrap();
    let preview = store
        .mission(
            &intent,
            IntentInput {
                kdl: source.clone(),
                source_name: None,
            },
        )
        .unwrap();
    assert!(preview.blockers.is_empty(), "{:?}", preview.blockers);
    let applied = store
        .apply_as(
            &intent,
            &preview.subject_tokens,
            "planning-session-publication",
            Some("person/operator"),
        )
        .unwrap();
    let session = store
        .planning_session("planning/release/one")
        .unwrap()
        .unwrap();
    let planner =
        crate::graph::planning_planner_subject("planning-session/planning/release/one");
    assert_eq!(session.status, "planning");
    assert_eq!(session.planner, planner);
    assert!(
        applied
            .operations
            .iter()
            .any(|operation| operation.action == "start-launch")
    );
    assert!(store.selected_desired_token(&planner).unwrap().is_some());
    // The planner seat starts idle; its instructions arrive as one launch request message.
    let requests = store.messages(Some(&planner), false).unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].from, "person/operator");
    assert_eq!(requests[0].title.as_deref(), Some("Launch request"));
    assert!(
        requests[0]
            .content
            .contains("st launch submit planning/release/one"),
        "{}",
        requests[0].content
    );

    let replay_preview = store
        .mission(
            &intent,
            IntentInput {
                kdl: source,
                source_name: None,
            },
        )
        .unwrap();
    let replay = store
        .apply_as(
            &intent,
            &replay_preview.subject_tokens,
            "planning-session-replay",
            Some("person/operator"),
        )
        .unwrap();
    assert!(!replay.changed);
}

#[test]
fn a_declared_human_revision_creates_and_approves_one_proposal() {
    let store = Store::open_memory("node").unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let initial = publish_mission(
        &store,
        r#"version 2
mission "reviewed" state="ready" revisions="human-only" {
  goal "Use the initial definition."
  step "work" { agentless }
}
"#,
        "publish-reviewed-initial",
    );
    let run = store
        .create_mission_run(&MissionRunRequest {
            mission: initial.id.clone(),
            revision: Some(initial.revision.clone()),
            workspace: workspace.path().display().to_string(),
            requester: Some("person/operator".into()),
            mode: None,
            inputs: BTreeMap::new(),
            idempotency_key: "run-reviewed".into(),
        })
        .unwrap();
    let candidate = publish_mission(
        &store,
        r#"version 2
mission "reviewed" state="ready" revisions="human-only" {
  goal "Use the reviewed definition."
  step "work" { agentless }
}
"#,
        "publish-reviewed-candidate",
    );
    let revision = format!(
        "version 2\nmission-run {:?} {{\n  revision \"review-one\" {{\n    mission {:?}\n    from {:?}\n    reason \"the definition needs review\"\n  }}\n}}\n",
        run.subject,
        format!("mission/{}@{}", candidate.id, candidate.revision),
        run.generation,
    );
    let intent = crate::graph::parse_intent(&revision, "node").unwrap();
    let preview = store
        .mission(
            &intent,
            IntentInput {
                kdl: revision,
                source_name: None,
            },
        )
        .unwrap();
    let applied = store
        .apply_as(
            &intent,
            &preview.subject_tokens,
            "declare-reviewed-revision",
            Some("person/operator"),
        )
        .unwrap();
    assert!(
        applied
            .operations
            .iter()
            .any(|operation| { operation.action == "propose-revision:review-one" })
    );
    let proposal = store.revision_proposal_for_run(&run.id).unwrap().unwrap();
    assert_eq!(proposal.status, "pending-approval");
    assert_eq!(proposal.reviewers, ["person/operator"]);
    let attention = store.attention_items(Some("person/operator")).unwrap();
    assert_eq!(attention.len(), 1);
    assert_eq!(attention[0].kind, "revision-approval");
    assert_eq!(attention[0].subject, proposal.subject);

    let approved = store
        .approve_revision_proposal(
            &proposal.id,
            "person/operator",
            proposal.preview_hash.as_deref().unwrap(),
            "approve-reviewed-revision",
        )
        .unwrap();
    assert_eq!(approved.status, "applied");
    assert_eq!(approved.mission_run.revision, candidate.revision);
    assert_ne!(approved.mission_run.generation, run.generation);
    assert!(
        store
            .attention_items(Some("person/operator"))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn a_mission_response_reports_the_server_normalized_revision() {
    let source = r#"
version 2

  mission "portable" state="ready" {
    goal "Keep one worker available."
     agent "worker" { workspace "."; command "true" }
  }

"#;
    let server_intent = crate::graph::parse_intent(source, "server-node").unwrap();
    let client_intent = crate::graph::parse_intent(source, "local").unwrap();
    let server_mission = server_intent.missions.values().next().unwrap();
    let client_mission = client_intent.missions.values().next().unwrap();
    assert_eq!(server_mission.revision, client_mission.revision);

    let store = Store::open_memory("server-node").unwrap();
    let response = store
        .mission(
            &server_intent,
            IntentInput {
                kdl: source.into(),
                source_name: None,
            },
        )
        .unwrap();
    assert_eq!(
        response.mission_revisions["mission/portable"],
        server_mission.revision
    );
}

#[test]
fn mission_inputs_are_exact_immutable_snapshots() {
    let store = Store::open_memory("node").unwrap();
    publish_mission(
        &store,
        r#"
version 2

  resource "source" { kind "custom.st3.document-source" }
  mission "inputs" state="ready" {
    input "message" kind="text"
    input "source" kind="resource"
    goal "Use the supplied values."
    step "work" { agentless; goal "Write ${input.message}."; gate "source state" { field "state" "${input.source}" is "ready" } }
  }

"#,
        "publish-input-mission",
    );
    let first = store
        .append_claim(&ClaimInput {
            subject: "resource/source".into(),
            kind: "resource.observed".into(),
            actor: None,
            fields: BTreeMap::from([("state".into(), Value::String("ready".into()))]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some("source-ready".into()),
        })
        .unwrap();
    let request = MissionRunRequest {
        mission: "inputs".into(),
        revision: None,
        workspace: ".".into(),
        requester: Some("person/test".into()),
        mode: None,
        inputs: BTreeMap::from([
            ("message".into(), "hello".into()),
            ("source".into(), "resource/source".into()),
        ]),
        idempotency_key: "run-input-mission".into(),
    };
    let run = store.create_mission_run(&request).unwrap();
    assert_eq!(run.inputs["message"].value, "hello");
    assert_eq!(
        run.inputs["source"].value,
        format!("resource/source@{}", first.id)
    );
    assert_eq!(
        store.create_mission_run(&request).unwrap().subject,
        run.subject
    );
    let mut second_run = request.clone();
    second_run.idempotency_key = "run-input-mission-two".into();
    assert_eq!(
        store.create_mission_run(&second_run).unwrap_err().code,
        "mission-run-capacity"
    );

    let mut changed = request.clone();
    changed.inputs.insert("message".into(), "changed".into());
    assert_eq!(
        store.create_mission_run(&changed).unwrap_err().code,
        "idempotency-mismatch"
    );
    let mut missing = request.clone();
    missing.idempotency_key = "run-input-missing".into();
    missing.inputs.remove("source");
    assert_eq!(
        store.create_mission_run(&missing).unwrap_err().code,
        "invalid-mission-inputs"
    );
    let mut extra = request.clone();
    extra.idempotency_key = "run-input-extra".into();
    extra.inputs.insert("other".into(), "value".into());
    assert_eq!(
        store.create_mission_run(&extra).unwrap_err().code,
        "invalid-mission-inputs"
    );

    let second = store
        .append_claim(&ClaimInput {
            subject: "resource/source".into(),
            kind: "resource.observed".into(),
            actor: None,
            fields: BTreeMap::from([("state".into(), Value::String("changed".into()))]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some("source-changed".into()),
        })
        .unwrap();
    assert_ne!(first.id, second.id);
    assert_eq!(
        store.mission_run(&run.id).unwrap().unwrap().inputs["source"].claim_id,
        Some(first.id)
    );
}

#[test]
fn child_runs_accept_inputs_and_revisions_cannot_change_them() {
    let store = Store::open_memory("node").unwrap();
    let source = r#"
version 2

  mission "parent" state="ready" { goal "Keep the parent run open." }
  mission "child" state="ready" {
    input "message" kind="text"
    goal "Use the supplied message."
  }

"#;
    let intent = parse_intent(source, "node").unwrap();
    let planned = store
        .mission(
            &intent,
            IntentInput {
                kdl: source.into(),
                source_name: None,
            },
        )
        .unwrap();
    store
        .apply(&intent, &planned.subject_tokens, "publish-parent-child")
        .unwrap();
    let parent = store
        .create_mission_run(&MissionRunRequest {
            mission: "parent".into(),
            revision: None,
            workspace: ".".into(),
            requester: None,
            mode: None,
            inputs: BTreeMap::new(),
            idempotency_key: "parent-run".into(),
        })
        .unwrap();
    let child = store
        .create_child_mission_run(
            &MissionRunRequest {
                mission: "child".into(),
                revision: None,
                workspace: ".".into(),
                requester: None,
                mode: None,
                inputs: BTreeMap::from([("message".into(), "hello".into())]),
                idempotency_key: "child-run".into(),
            },
            &parent,
            "step-run/test/child",
            None,
        )
        .unwrap();
    assert_eq!(child.inputs["message"].value, "hello");
    assert_eq!(child.root_mission_run, parent.subject);

    let original = publish_mission(
        &store,
        r#"
version 2

  mission "revision-input" state="ready" {
    input "message" kind="text"
    goal "Use the supplied message."
  }

"#,
        "publish-revision-input",
    );
    let run = store
        .create_mission_run(&MissionRunRequest {
            mission: original.id,
            revision: None,
            workspace: ".".into(),
            requester: None,
            mode: None,
            inputs: BTreeMap::from([("message".into(), "hello".into())]),
            idempotency_key: "revision-input-run".into(),
        })
        .unwrap();
    let changed = publish_mission(
        &store,
        r#"
version 2

  mission "revision-input" state="ready" {
    input "source" kind="resource"
    goal "Use the supplied resource."
  }

"#,
        "publish-changed-revision-input",
    );
    let revision_error = store
        .adopt_mission_revision(
            &run.id,
            &changed,
            "agent/node.worker",
            "change the input declaration",
            "adopt-changed-revision-input",
        )
        .unwrap_err();
    assert_eq!(revision_error.code, "run-input-mutation");
}

#[test]
fn mission_run_capacity_uses_the_strictest_active_revision() {
    let store = Store::open_memory("node").unwrap();
    let first = publish_mission(
        &store,
        r#"version 2
 mission "bounded" state="ready" { concurrent-runs max=3; goal "Keep this run open." } "#,
        "bounded-three",
    );
    for index in 1..=2 {
        store
            .create_mission_run(&MissionRunRequest {
                mission: "bounded".into(),
                revision: Some(first.revision.clone()),
                workspace: ".".into(),
                requester: None,
                mode: None,
                inputs: BTreeMap::new(),
                idempotency_key: format!("bounded-run-{index}"),
            })
            .unwrap();
    }
    let second = publish_mission(
        &store,
        r#"version 2
 mission "bounded" state="ready" { concurrent-runs max=1; goal "Keep one run open." } "#,
        "bounded-one",
    );
    assert_eq!(
        store
            .active_mission_runs_for_mission("bounded")
            .unwrap()
            .len(),
        2
    );
    let error = store
        .create_mission_run(&MissionRunRequest {
            mission: "bounded".into(),
            revision: Some(second.revision),
            workspace: ".".into(),
            requester: None,
            mode: None,
            inputs: BTreeMap::new(),
            idempotency_key: "bounded-run-three".into(),
        })
        .unwrap_err();
    assert_eq!(error.code, "mission-run-capacity");
    assert!(error.message.contains("mission-run/"));

    let unlimited = Store::open_memory("node").unwrap();
    let mission = publish_mission(
        &unlimited,
        r#"version 2
 mission "unlimited" state="ready" { concurrent-runs; goal "Allow all runs." } "#,
        "unlimited-mission",
    );
    for index in 1..=3 {
        unlimited
            .create_mission_run(&MissionRunRequest {
                mission: "unlimited".into(),
                revision: Some(mission.revision.clone()),
                workspace: ".".into(),
                requester: None,
                mode: None,
                inputs: BTreeMap::new(),
                idempotency_key: format!("unlimited-run-{index}"),
            })
            .unwrap();
    }
}

#[test]
fn terminal_operations_do_not_replace_member_status() {
    let store = Store::open_memory("node").expect("store");
    let subject = "pty/demo";
    store
        .append_claim(&ClaimInput {
            subject: subject.into(),
            kind: "runtime.observed".into(),
            actor: None,
            fields: BTreeMap::from([("status".into(), Value::String("running".into()))]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .expect("member observation");
    store
        .append_claim(&ClaimInput {
            subject: subject.into(),
            kind: "terminal.input.result".into(),
            actor: None,
            fields: BTreeMap::from([("result".into(), Value::String("written".into()))]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .expect("input result");

    let actual = store
        .latest_actual_value(subject)
        .expect("actual state")
        .expect("subject state");
    assert_eq!(actual["status"], "running");
    assert_eq!(actual["result"], "written");
}

#[test]
fn old_document_versions_remain_valid() {
    let store = Store::open_memory("node").expect("store");
    let old = store
        .put_document("doc/task", b"old", &None, "old")
        .expect("old");
    let new = store
        .put_document(
            "doc/task",
            b"new",
            &Some(old.binding_claim_id.clone()),
            "new",
        )
        .expect("new");
    assert_ne!(old.hash, new.hash);
    let selected = store
        .list_documents(Some("doc/task"), false, 10)
        .expect("selected documents");
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].hash, new.hash);
    let history = store
        .list_documents(Some("doc/task"), true, 10)
        .expect("document history");
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].hash, new.hash);
    assert_eq!(history[1].hash, old.hash);
    assert_eq!(
        store
            .list_documents(Some("doc/task"), true, 1)
            .expect("bounded history")
            .len(),
        1
    );
    let intent = parse_intent(
        &format!(
            "version 2\n person \"worker\"; message \"task\" {{ to \"person/worker\"; content \"doc/task@{}\"; }} ",
            old.hash
        ),
        "node",
    )
    .expect("intent");
    let mission = store
        .mission(
            &intent,
            IntentInput {
                kdl: "test".into(),
                source_name: None,
            },
        )
        .expect("mission");
    assert!(mission.blockers.is_empty());
    assert_eq!(mission.warnings.len(), 1);
}

#[test]
fn historical_mission_and_status_use_the_selected_index() {
    let store = Store::open_memory("node").expect("store");
    let first = simple("true");
    let first_mission = store
        .mission(
            &first,
            IntentInput {
                kdl: "first".into(),
                source_name: None,
            },
        )
        .unwrap();
    let first_apply = store
        .apply(&first, &first_mission.subject_tokens, "first")
        .unwrap();
    let first_index = first_apply.store_index;

    let second = simple("false");
    let second_mission = store
        .mission(
            &second,
            IntentInput {
                kdl: "second".into(),
                source_name: None,
            },
        )
        .unwrap();
    store
        .apply(&second, &second_mission.subject_tokens, "second")
        .unwrap();

    let historical = store
        .status_at(Some("exec/work"), None, Some(first_index))
        .unwrap();
    assert_eq!(historical.store_index, first_index);
    assert_eq!(
        historical.subjects[0].desired_token,
        first_apply.subject_tokens["exec/work"].first().cloned()
    );
    let historical_mission = store
        .mission_at(
            &second,
            IntentInput {
                kdl: "historical".into(),
                source_name: None,
            },
            Some(first_index),
        )
        .unwrap();
    assert_eq!(
        historical_mission.subject_tokens["exec/work"],
        first_apply.subject_tokens["exec/work"]
    );
    assert_eq!(historical_mission.changes.len(), 1);
}

#[test]
fn snapshot_operational_annotations_follow_owner_claims_at_the_same_index() {
    let store = Store::open_memory("node").expect("store");
    publish_mission(
        &store,
        r#"version 2
mission "snapshot-owner" state="ready" {
  goal "Fence operational history to the selected graph index."
  step "work" { agentless }
}"#,
        "snapshot-owner-mission",
    );
    let run = store
        .create_mission_run(&MissionRunRequest {
            mission: "snapshot-owner".into(),
            revision: None,
            workspace: "/tmp".into(),
            requester: Some("person/test".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "snapshot-owner-run".into(),
        })
        .unwrap();

    let mut owned = simple("true");
    let desired = owned.subjects.get_mut("exec/work").unwrap();
    desired.owner_run = Some(run.subject.clone());
    desired.owner_generation = Some(run.generation.clone());
    let planned = store
        .mission(
            &owned,
            IntentInput {
                kdl: "owned runtime".into(),
                source_name: None,
            },
        )
        .unwrap();
    let applied = store
        .apply(&owned, &planned.subject_tokens, "snapshot-owned-runtime")
        .unwrap();
    let current_index = applied.store_index;
    let current = store
        .status_at(Some("exec/work"), None, Some(current_index))
        .unwrap();
    assert_eq!(current.subjects[0].projection.layer, "current");

    store
        .append_claim(&ClaimInput {
            subject: run.generation.clone(),
            kind: "run-generation.superseded".into(),
            actor: Some("person/test".into()),
            fields: BTreeMap::from([
                ("status".into(), Value::String("superseded".into())),
                (
                    "successor".into(),
                    Value::String("run-generation/replacement".into()),
                ),
                ("reason".into(), Value::String("test revision".into())),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    let superseded_index = store.index().unwrap();
    let superseded = store
        .status_at(Some("exec/work"), None, Some(superseded_index))
        .unwrap();
    assert_eq!(superseded.subjects[0].projection.layer, "history");
    assert!(!superseded.subjects[0].projection.actionable);
    assert!(
        superseded.subjects[0]
            .projection
            .reasons
            .contains(&"superseded".to_owned())
    );

    let replacement = "run-generation/replacement";
    store
        .append_claim(&ClaimInput {
            subject: replacement.into(),
            kind: "run-generation.created".into(),
            actor: Some("person/test".into()),
            fields: BTreeMap::from([
                ("run".into(), Value::String(run.subject.clone())),
                (
                    "revision".into(),
                    Value::String("replacement-revision".into()),
                ),
                ("status".into(), Value::String("running".into())),
                ("reason".into(), Value::String("test revision".into())),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    let mut successor_owned = simple("false");
    let desired = successor_owned.subjects.get_mut("exec/work").unwrap();
    desired.owner_run = Some(run.subject.clone());
    desired.owner_generation = Some(replacement.into());
    let planned = store
        .mission(
            &successor_owned,
            IntentInput {
                kdl: "successor-owned runtime".into(),
                source_name: None,
            },
        )
        .unwrap();
    let successor = store
        .apply(
            &successor_owned,
            &planned.subject_tokens,
            "snapshot-successor-owned-runtime",
        )
        .unwrap();
    let successor_status = store
        .status_at(Some("exec/work"), None, Some(successor.store_index))
        .unwrap();
    assert_eq!(successor_status.subjects[0].projection.layer, "current");
    assert!(
        !successor_status.subjects[0]
            .projection
            .reasons
            .contains(&"superseded".to_owned())
    );

    assert!(
        store
            .set_mission_run_state(
                &run.id,
                "cancelled",
                "terminal",
                Some("the owner completed its lifecycle"),
            )
            .unwrap()
    );
    let terminal_index = store.index().unwrap();
    let before_terminal = store
        .status_at(Some("exec/work"), None, Some(current_index))
        .unwrap();
    assert_eq!(before_terminal.subjects[0].projection.layer, "current");
    let terminal = store
        .status_at(Some("exec/work"), None, Some(terminal_index))
        .unwrap();
    assert_eq!(terminal.subjects[0].projection.layer, "history");
    assert!(!terminal.subjects[0].projection.actionable);
    assert!(
        terminal.subjects[0]
            .projection
            .reasons
            .contains(&"terminal-owner".to_owned())
    );
    assert!(
        store
            .status_at(None, None, Some(terminal_index))
            .unwrap()
            .subjects
            .iter()
            .all(|subject| subject.subject != "exec/work")
    );
    let historical = store
        .status_history(None, None, Some(terminal_index))
        .unwrap();
    let owned = historical
        .subjects
        .iter()
        .find(|subject| subject.subject == "exec/work")
        .unwrap();
    assert_eq!(owned.projection.layer, "history");
    assert!(!owned.projection.actionable);
}

#[test]
fn replication_carries_document_bytes_and_name_versions() {
    let source = Store::open_memory("source").unwrap();
    let document = source
        .put_document("doc/task", b"replicated", &None, "document")
        .unwrap();
    let batch = source.export_replication(0).unwrap();
    let target = Store::open_memory("target").unwrap();
    target.import_replication("source", &batch).unwrap();
    assert_eq!(
        target
            .get_document("doc/task", &document.hash)
            .unwrap()
            .unwrap(),
        b"replicated"
    );
}

#[test]
fn replication_carries_loop_state_and_round_results() {
    let source = Store::open_memory("source").unwrap();
    let subject = "loop-run/01990000000070008000000000000000/improve";
    source
        .append_claim(&ClaimInput {
            subject: subject.into(),
            kind: "loop.state".into(),
            actor: None,
            fields: BTreeMap::from([
                ("status".into(), Value::String("running".into())),
                ("round".into(), Value::from(1)),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some("replicate-loop-state".into()),
        })
        .unwrap();
    source
        .append_claim(&ClaimInput {
            subject: subject.into(),
            kind: "loop.round-result".into(),
            actor: None,
            fields: BTreeMap::from([
                ("round".into(), Value::from(1)),
                ("status".into(), Value::String("completed".into())),
                (
                    "mission_run".into(),
                    Value::String("mission-run/01990000000070008000000000000001".into()),
                ),
                ("metrics".into(), serde_json::json!({"quality": 1})),
                ("token_usage".into(), Value::from(10)),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some("replicate-loop-result".into()),
        })
        .unwrap();

    let target = Store::open_memory("target").unwrap();
    target
        .import_replication("source", &source.export_replication(0).unwrap())
        .unwrap();

    assert_eq!(
        target
            .latest_claim(subject, Some("loop.state"))
            .unwrap()
            .unwrap()
            .body
            .pointer("/fields/round")
            .and_then(Value::as_u64),
        Some(1)
    );
    assert_eq!(
        target
            .claims_for(subject, Some("loop.round-result"))
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn a_host_document_must_exist_before_its_declaration_is_applied() {
    let store = Store::open_memory("node").unwrap();
    let bytes = b"host facts\n";
    let hash = hex::encode(sha2::Sha256::digest(bytes));
    let source = format!("version 2\nhost \"node\" {{ document \"doc/hosts/node@{hash}\" }}\n");
    let intent = crate::graph::parse_test_intent(&source, "node").unwrap();
    let preview = store
        .mission(
            &intent,
            IntentInput {
                kdl: source.clone(),
                source_name: None,
            },
        )
        .unwrap();
    let error = store
        .apply(&intent, &preview.subject_tokens, "missing-host-document")
        .unwrap_err();
    assert_eq!(error.code, "missing-document");

    store
        .put_document("doc/hosts/node", bytes, &None, "host-document")
        .unwrap();
    let preview = store
        .mission(
            &intent,
            IntentInput {
                kdl: source,
                source_name: None,
            },
        )
        .unwrap();
    store
        .apply(&intent, &preview.subject_tokens, "apply-host-document")
        .unwrap();
}

#[test]
fn replication_carries_runnable_mission_definitions() {
    let source = Store::open_memory("source").unwrap();
    let kdl = r#"version 2
 mission "remote" state="ready" { goal "Run remote work."; step "work" { } } "#;
    let intent = parse_intent(kdl, "source").unwrap();
    let planned = source
        .mission(
            &intent,
            IntentInput {
                kdl: kdl.into(),
                source_name: None,
            },
        )
        .unwrap();
    source
        .apply(&intent, &planned.subject_tokens, "remote-mission")
        .unwrap();

    let target = Store::open_memory("target").unwrap();
    target
        .import_replication("source", &source.export_replication(0).unwrap())
        .unwrap();
    let replicated = target.mission_spec("remote", None).unwrap().unwrap();
    assert_eq!(replicated.state, MissionState::Ready);
    let run = target
        .create_mission_run(&MissionRunRequest {
            mission: "remote".into(),
            revision: Some(replicated.revision),
            workspace: "/tmp".into(),
            requester: Some("person/test".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "remote-run".into(),
        })
        .unwrap();
    assert_eq!(run.steps.len(), 1);
}

#[test]
fn replication_carries_mission_runs_and_remote_work_updates() {
    let controller = Store::open_memory("controller").unwrap();
    let kdl = r#"
version 2

  mission "remote-work" state="ready" {
    goal "Complete mission remote-work."
    step "work" { assigned-to "agent/worker.one" }
  }

"#;
    let intent = parse_intent(kdl, "controller").unwrap();
    let planned = controller
        .mission(
            &intent,
            IntentInput {
                kdl: kdl.into(),
                source_name: None,
            },
        )
        .unwrap();
    controller
        .apply(&intent, &planned.subject_tokens, "remote-work-mission")
        .unwrap();
    let run = controller
        .create_mission_run(&MissionRunRequest {
            mission: "remote-work".into(),
            revision: None,
            workspace: "/tmp".into(),
            requester: Some("person/test".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "remote-work-run".into(),
        })
        .unwrap();
    let step = run.steps[0].subject.clone();
    controller.set_step_state(&step, "ready", None).unwrap();

    let worker = Store::open_memory("worker").unwrap();
    receive_and_project(
        &worker,
        "controller",
        &exchange_from(&controller, &ReplicationInventory::default()),
    );
    let remote = worker.mission_run(&run.id).unwrap().unwrap();
    assert_eq!(remote.steps[0].status, "ready");
    assert!(controller.project_replication_backlog().unwrap());
    controller.bind_fleet(TEST_FLEET).unwrap();
    for action in ["claim", "complete"] {
        worker
            .work_action(
                &step,
                action,
                &WorkRequest {
                    wait_ms: None,
                    actor: Some("agent/worker.one".into()),
                    incarnation: Some("worker-generation".into()),
                    summary: Some("the remote work is complete".into()),
                    reason: None,
                    evidence: Vec::new(),
                    idempotency_key: format!("remote-{action}"),
                },
            )
            .unwrap();
        let exchange = exchange_from(&worker, &controller.replication_inventory().unwrap());
        controller
            .receive_replication_exchange("worker", TEST_FLEET, &exchange)
            .unwrap();
        controller.validate_replication_backlog().unwrap();
        controller.apply_replication_repairs().unwrap();
        {
            let mut connection = controller.connection.lock().unwrap();
            let transaction = connection.transaction().unwrap();
            assert!(
                try_project_all_simple_replication_tx(&transaction, &controller.origin).unwrap(),
                "a single routine work transition should use the bounded projection path"
            );
            transaction.rollback().unwrap();
        }
        assert!(controller.project_replication_backlog().unwrap());
    }
    assert_eq!(
        controller.step_run(&step).unwrap().unwrap().status,
        "verifying"
    );
}

/// A controller that publishes a one-step mission and a worker that projects its run, with
/// the step ready for the worker's agent.
fn replicated_step_pair() -> (Store, Store, String) {
    let controller = Store::open_memory("controller").unwrap();
    let kdl = r#"
version 2

  mission "lease-work" state="ready" {
    goal "Complete mission lease-work."
    step "work" { assigned-to "agent/worker.one" }
  }

"#;
    let intent = parse_intent(kdl, "controller").unwrap();
    let planned = controller
        .mission(
            &intent,
            IntentInput {
                kdl: kdl.into(),
                source_name: None,
            },
        )
        .unwrap();
    controller
        .apply(&intent, &planned.subject_tokens, "lease-work-mission")
        .unwrap();
    let run = controller
        .create_mission_run(&MissionRunRequest {
            mission: "lease-work".into(),
            revision: None,
            workspace: "/tmp".into(),
            requester: Some("person/test".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "lease-work-run".into(),
        })
        .unwrap();
    let step = run.steps[0].subject.clone();
    controller.set_step_state(&step, "ready", None).unwrap();
    let worker = Store::open_memory("worker").unwrap();
    receive_and_project(
        &worker,
        "controller",
        &exchange_from(&controller, &ReplicationInventory::default()),
    );
    assert!(controller.project_replication_backlog().unwrap());
    (controller, worker, step)
}

fn worker_work(worker: &Store, step: &str, action: &str, summary: Option<&str>, key: &str) {
    worker
        .work_action(
            step,
            action,
            &WorkRequest {
                wait_ms: None,
                actor: Some("agent/worker.one".into()),
                incarnation: Some("worker-generation".into()),
                summary: summary.map(str::to_owned),
                reason: None,
                evidence: Vec::new(),
                idempotency_key: key.into(),
            },
        )
        .unwrap();
}

fn worker_extend(worker: &Store, step: &str) -> StepRunView {
    worker
        .work_action_extending(
            step,
            "extend",
            &WorkRequest {
                wait_ms: None,
                actor: Some("agent/worker.one".into()),
                incarnation: Some("worker-generation".into()),
                summary: None,
                reason: Some("the invented test needs more time".into()),
                evidence: Vec::new(),
                idempotency_key: "extension-budget".into(),
            },
            Some(300_000),
        )
        .unwrap()
}

/// Receive `source`'s new envelopes and project them, and say whether that replayed the
/// graph from nothing.
fn projection_replayed(target: &Store, relay: &str, source: &Store) -> bool {
    let exchange = exchange_from(source, &target.replication_inventory().unwrap());
    target
        .receive_replication_exchange(relay, TEST_FLEET, &exchange)
        .unwrap();
    target.validate_replication_backlog().unwrap();
    target.apply_replication_repairs().unwrap();
    FULL_REPLAYS.with(|replays| replays.set(0));
    assert!(target.project_replication_backlog().unwrap());
    FULL_REPLAYS.with(std::cell::Cell::get) > 0
}

#[test]
fn cache_receipt_work_progress_and_complete_do_not_replay_either_node() {
    let (controller, worker, step) = replicated_step_pair();
    for action in ["claim", "progress", "complete"] {
        worker_work(
            &worker,
            &step,
            action,
            Some("the invented work advances"),
            &format!("cache-receipt-{action}"),
        );
        let claim = worker
            .claims_for(&step, None)
            .unwrap()
            .into_iter()
            .max_by_key(|claim| claim.store_index)
            .unwrap();
        assert!(
            operation_parts(&claim.body)
                .is_some_and(|(id, _)| id.starts_with("cache/")),
            "the actual work endpoint must attach its durable receipt association"
        );

        FULL_REPLAYS.with(|count| count.set(0));
        assert!(worker.project_replication_backlog().unwrap());
        assert_eq!(FULL_REPLAYS.with(std::cell::Cell::get), 0, "local {action}");
        assert!(
            !projection_replayed(&controller, "worker", &worker),
            "follower {action}"
        );
        assert_eq!(
            graph_digest_of(&worker),
            graph_digest_of(&controller),
            "both nodes must project the same actual work transition"
        );
    }
    assert_eq!(worker.step_run(&step).unwrap().unwrap().status, "verifying");
    assert_eq!(
        controller.step_run(&step).unwrap().unwrap().status,
        "verifying"
    );
}

#[test]
fn work_nudged_operations_are_incremental_in_opposite_arrival_and_batch_orders() {
    fn nudge(store: &Store, step: &str, key: &str) -> ClaimRecord {
        store.append_claim(&ClaimInput {
            subject: step.into(),
            kind: WORK_NUDGED_KIND.into(),
            actor: Some("daemon/runtime".into()),
            fields: BTreeMap::from([
                ("attempt".into(), Value::from(1)),
                ("agent".into(), Value::String("agent/worker.one".into())),
                ("message".into(), Value::String("message/invented-nudge".into())),
                ("idle_since_unix_ms".into(), Value::String("1".into())),
                ("reason".into(), Value::String(key.into())),
                ("waits".into(), json!([])),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some(key.into()),
        }).unwrap()
    }
    fn stored_step(store: &Store, step: &str) -> String {
        store.readers.get().query_row(
            "SELECT json_array(status,attempt,worker_reported,lease_owner,
                 lease_incarnation,lease_expires_at_unix_ms,blocked_reason,updated_at_unix_ms)
                 FROM step_runs WHERE subject=?1",
            [step], |row| row.get(0),
        ).unwrap()
    }
    for split_worker_batches in [false, true] {
        let (controller, worker, step) = replicated_step_pair();
        worker_work(&worker, &step, "claim", None, "nudge-claim");
        assert!(!projection_replayed(&controller, "worker", &worker));
        assert!(worker.project_replication_backlog().unwrap());
        // These private nodes sign the new nudge claims through the normal sealer.
        for source in [&controller, &worker] {
            source.set_node_key(Arc::new(smallclaims::fleet::MemberKey::generate().unwrap().0)).unwrap();
        }
        let left = Store::open_memory("left").unwrap();
        let right = Store::open_memory("right").unwrap();
        for follower in [&left, &right] {
            receive_and_project(follower, "controller",
                &exchange_from(&controller, &ReplicationInventory::default()));
        }
        for store in [&controller, &worker, &left, &right] {
            assert!(!store.replication_projection_needs_recovery().unwrap(),
                "initialize healthy projections before the nudge exchange");
        }
        let before = stored_step(&controller, &step);
        let before_index = left.index().unwrap();
        let local = nudge(&controller, &step, "nudge-controller");
        let foreign_first = nudge(&worker, &step, "nudge-worker-first");
        // Seal a first worker batch before admitting a second. One follower may
        // project the first batch separately; the other receives both together.
        let first_exchange = exchange_from(&worker, &left.replication_inventory().unwrap());
        if split_worker_batches {
            FULL_REPLAYS.with(|count| count.set(0));
            receive_and_project(&left, "worker", &first_exchange);
            assert_eq!(FULL_REPLAYS.with(std::cell::Cell::get), 0);
        }
        let foreign_second = nudge(&worker, &step, "nudge-worker-second");
        for source in [&controller, &worker] {
            FULL_REPLAYS.with(|count| count.set(0));
            assert!(source.project_replication_backlog().unwrap());
            assert_eq!(FULL_REPLAYS.with(std::cell::Cell::get), 0, "local nudge");
        }
        // The right follower sees worker batches before the controller's
        // nudge, the left sees the controller before the remaining worker data.
        assert!(!projection_replayed(&right, "worker", &worker));
        assert!(!projection_replayed(&right, "controller", &controller));
        assert!(!projection_replayed(&left, "controller", &controller));
        assert!(!projection_replayed(&left, "worker", &worker));
        assert!(!left.claims_since_only_quiet_notifications(before_index).unwrap(),
            "a nudge must retain the existing receive wake classification");
        let nudges = [&local, &foreign_first, &foreign_second];
        let latest = nudges.iter().map(|claim| claim.accepted_at_unix_ms).max().unwrap();
        // Both arrival orders must use identical canonical authority.
        let left_connection = left.readers.get();
        let right_connection = right.readers.get();
        for claim in &nudges {
            assert_eq!(canonical::claim_key(&left_connection, &claim.id).unwrap(),
                canonical::claim_key(&right_connection, &claim.id).unwrap());
        }
        drop(left_connection);
        drop(right_connection);
        let mut digests = Vec::new();
        for follower in [&left, &right] {
            assert_eq!(stored_step(follower, &step), before, "nudge is not a state transition");
            let connection = follower.readers.get();
            let mut view = follower.step_run(&step).unwrap().unwrap();
            enrich_step_nudge_at(&connection, &mut view, latest).unwrap();
            assert_eq!(view.nudged_at_unix_ms, Some(latest));
            assert_eq!(follower.latest_nudge(&step).unwrap().unwrap().accepted_at_unix_ms, latest);
            let rows = nudges.iter().map(|claim| {
                let (operation, _) = operation_parts(&claim.body).expect("durable operation");
                let signed: bool = connection.query_row(
                    "SELECT EXISTS(SELECT 1 FROM claim_signatures WHERE claim_id=?1)",
                    [&claim.id], |row| row.get(0),
                ).unwrap();
                assert!(signed, "replicated nudge carries its stored signature");
                let row = operation_tx(&connection, operation).unwrap().unwrap();
                assert_eq!(row.2, "active");
                row
            }).collect::<Vec<_>>();
            drop(connection);
            assert!(follower.operation_projection_drift().unwrap().is_empty());
            let digest = graph_digest_of(follower);
            follower.replay_replication_graph().unwrap();
            assert_eq!(graph_digest_of(follower), digest, "full replay oracle");
            assert_eq!(stored_step(follower, &step), before);
            let connection = follower.readers.get();
            let mut replayed_view = follower.step_run(&step).unwrap().unwrap();
            enrich_step_nudge_at(&connection, &mut replayed_view, latest).unwrap();
            assert_eq!(replayed_view.nudged_at_unix_ms, Some(latest));
            for (claim, expected) in nudges.iter().zip(rows) {
                let (operation, _) = operation_parts(&claim.body).unwrap();
                assert_eq!(operation_tx(&connection, operation).unwrap().unwrap(), expected);
            }
            digests.push(digest);
        }
        assert_eq!(digests[0], digests[1]);
    }
}

#[test]
fn work_nudged_incremental_guards_and_rollback_preserve_the_committed_frontier() {
    for refusal in ["none", "malformed-operation", "operation-conflict", "unhealthy-projection"] {
        let store = Store::open_memory("node").unwrap();
        assert!(store.project_replication_backlog().unwrap());
        let before = store.index().unwrap();
        let digest = graph_digest_of(&store);
        let mut connection = store.connection.lock().unwrap();
        let transaction = connection.transaction().unwrap();
        let frontier: u64 = transaction.query_row(
            "SELECT last_good_store_index FROM projection_health WHERE aggregate='graph'",
            [], |row| row.get(0),
        ).unwrap();
        let mut body = json!({"fields": {
            "attempt": 1, "agent": "agent/worker.one", "message": "message/invented-nudge",
            "idle_since_unix_ms": "1", "reason": "invented control", "waits": []
        }, "_operation": {"id": "op/nudge-guard", "request_digest": "digest-a"}});
        if refusal == "malformed-operation" {
            body["_operation"] = json!({"id": "op/nudge-guard"});
        }
        let claim = append_claim_record_tx(&transaction, "node", "step-run/invented",
            WORK_NUDGED_KIND, Some("daemon/runtime"), &body, &[], None).unwrap();
        if refusal == "operation-conflict" {
            transaction.execute(
                "INSERT INTO operations(id,request_digest,canonical_claim_id,state)
                     VALUES ('op/nudge-guard','digest-b',?1,'active')", [&claim.id],
            ).unwrap();
        }
        if refusal == "unhealthy-projection" {
            transaction.execute("UPDATE projection_health SET status='stale' WHERE aggregate='graph'", []).unwrap();
        }
        let result = try_project_simple_replication_tx(&transaction, &store.origin, claim.store_index).unwrap();
        match result {
            IncrementalProjection::Projected => {
                assert_eq!(refusal, "none");
                assert_eq!(operation_tx(&transaction, "op/nudge-guard").unwrap().unwrap().2, "active");
            }
            IncrementalProjection::Replay(reason)
            | IncrementalProjection::ReplayWithContext { reason, .. } => assert_eq!(reason, refusal),
        }
        // A projection attempt never advertises the uncommitted index as good.
        assert_eq!(transaction.query_row(
            "SELECT last_good_store_index FROM projection_health WHERE aggregate='graph'",
            [], |row| row.get::<_, u64>(0),
        ).unwrap(), frontier);
        transaction.rollback().unwrap();
        assert_eq!(operation_tx(&connection, "op/nudge-guard").unwrap(), None);
        drop(connection);
        assert_eq!(store.index().unwrap(), before);
        assert_eq!(graph_digest_of(&store), digest);
        assert!(!store.replication_projection_needs_recovery().unwrap());
    }
}

#[test]
fn work_nudged_historical_status_is_ignored_by_incremental_tree_and_full_replay() {
    let (store, _worker, step) = replicated_step_pair();
    assert!(!store.replication_projection_needs_recovery().unwrap());
    let rejected = store.append_claim(&ClaimInput {
        subject: step.clone(), kind: WORK_NUDGED_KIND.into(),
        actor: Some("daemon/runtime".into()),
        fields: BTreeMap::from([
            ("attempt".into(), json!(1)), ("agent".into(), json!("agent/worker.one")),
            ("message".into(), json!("message/historical-nudge")),
            ("idle_since_unix_ms".into(), json!("1")),
            ("reason".into(), json!("historical control")), ("waits".into(), json!([])),
            ("status".into(), json!("failed")),
        ]),
        evidence: Vec::new(), expected_subject: None,
        idempotency_key: Some("historical-nudge-admission-refusal".into()),
    }).unwrap_err();
    assert_eq!(rejected.code, "unknown-claim-field");
    let mut connection = store.connection.lock().unwrap();
    let transaction = connection.transaction().unwrap();
    let stored_step = |connection: &rusqlite::Connection| -> String {
        connection.query_row(
            "SELECT json_array(status,attempt,worker_reported,lease_owner,
                 lease_incarnation,lease_expires_at_unix_ms,blocked_reason,updated_at_unix_ms)
                 FROM step_runs WHERE subject=?1",
            [&step], |row| row.get(0),
        ).unwrap()
    };
    let before = stored_step(&transaction);
    // A retained-history fixture deliberately bypasses current admission,
    // which rejects status and lease fields on work.nudged.
    let body = json!({"fields": {
        "attempt": 1, "agent": "agent/worker.one", "message": "message/historical-nudge",
        "idle_since_unix_ms": "1", "reason": "historical control", "waits": [],
        "status": "failed", "lease_owner": "agent/historical", "worker_reported": true
    }, "_operation": {"id": "op/historical-nudge", "request_digest": "historical-digest"}});
    let claim = append_claim_record_tx(&transaction, "historical", &step,
        WORK_NUDGED_KIND, Some("daemon/runtime"), &body, &[], None).unwrap();
    project_mission_run_update(&transaction, &claim).unwrap();
    assert_eq!(stored_step(&transaction), before, "direct incremental state update ignores nudge");
    FULL_REPLAYS.with(|count| count.set(0));
    assert!(matches!(
        try_project_simple_replication_tx(&transaction, &store.origin, claim.store_index).unwrap(),
        IncrementalProjection::Projected
    ));
    assert_eq!(FULL_REPLAYS.with(std::cell::Cell::get), 0);
    assert_eq!(stored_step(&transaction), before);
    let incremental_digest = graph_digest(&transaction).unwrap();
    let operation = operation_tx(&transaction, "op/historical-nudge").unwrap().unwrap();
    assert_eq!(operation.2, "active");
    let root = run_tree_of_tx(&transaction, &step).unwrap().unwrap();
    rebuild_run_tree_tx(&transaction, &root).unwrap();
    assert_eq!(stored_step(&transaction), before, "affected-tree replay ignores nudge state");
    assert_eq!(graph_digest(&transaction).unwrap(), incremental_digest);
    replay_graph_from_nothing_tx(&transaction).unwrap();
    assert_eq!(stored_step(&transaction), before, "full replay ignores nudge state");
    assert_eq!(graph_digest(&transaction).unwrap(), incremental_digest);
    assert_eq!(operation_tx(&transaction, "op/historical-nudge").unwrap().unwrap(), operation);
    assert_eq!(claim_by_id_tx(&transaction, &claim.id).unwrap().unwrap().body, body);
    transaction.rollback().unwrap();
}

#[test]
fn supported_work_operations_do_not_require_a_cache_prefix() {
    for operation in ["op/invented-work", "cache/invented-work"] {
        let (controller, worker, step) = replicated_step_pair();
        worker_work(&worker, &step, "claim", None, "work-operation-source");
        let original = worker
            .claims_for(&step, Some("work.claimed"))
            .unwrap()
            .pop()
            .unwrap();
        let mut body = original.body.clone();
        body["_operation"] = json!({"id": operation, "request_digest": "invented-digest"});
        {
            let mut connection = controller.connection.lock().unwrap();
            let transaction = connection.transaction().unwrap();
            // Historical admitted input with the actual endpoint's transition fields;
            // changing metadata here is not a production admission bypass.
            append_claim_record_tx(
                &transaction,
                "worker",
                &step,
                "work.claimed",
                original.actor.as_deref(),
                &body,
                &[],
                None,
            )
            .unwrap();
            transaction.commit().unwrap();
        }
        FULL_REPLAYS.with(|count| count.set(0));
        assert!(controller.project_replication_backlog().unwrap());
        assert_eq!(FULL_REPLAYS.with(std::cell::Cell::get), 0, "{operation}");
        assert_eq!(
            controller.step_run(&step).unwrap().unwrap().status,
            "claimed"
        );
        assert!(controller.operation_projection_drift().unwrap().is_empty());
        let incremental = graph_digest_of(&controller);
        controller.replay_replication_graph().unwrap();
        assert_eq!(incremental, graph_digest_of(&controller), "{operation}");

        // A conflicting identity still requires canonical replay and names its work claim.
        body["_operation"]["request_digest"] = json!("different-digest");
        let incoming = {
            let mut connection = controller.connection.lock().unwrap();
            let transaction = connection.transaction().unwrap();
            let claim = append_claim_record_tx(
                &transaction,
                "worker",
                &step,
                "work.claimed",
                original.actor.as_deref(),
                &body,
                &[],
                None,
            )
            .unwrap();
            transaction.commit().unwrap();
            claim
        };
        FULL_REPLAYS.with(|count| count.set(0));
        let mut lines = Vec::new();
        assert!(
            controller
                .project_replication_backlog_with_log("test", |line| lines.push(line.to_owned()))
                .unwrap()
        );
        assert_eq!(FULL_REPLAYS.with(std::cell::Cell::get), 1);
        let detail: Value = serde_json::from_str(
            lines
                .iter()
                .find_map(|line| line.strip_prefix("st: projection failure detail "))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(detail["code"], "operation-conflict");
        assert_eq!(detail["claim_kind"], "work.claimed");
        assert_eq!(detail["claim_id"], incoming.id);
        assert_eq!(detail["operation_id"], operation);
        assert!(controller.operation_projection_drift().unwrap().is_empty());
        let conflicted = graph_digest_of(&controller);
        controller.replay_replication_graph().unwrap();
        assert_eq!(conflicted, graph_digest_of(&controller));
    }
}

#[test]
fn cache_receipt_work_lifecycle_matches_replay_on_updated_peers() {
    for action in ["renew", "progress", "release", "fail", "complete"] {
        let (controller, worker, step) = replicated_step_pair();
        worker_work(&worker, &step, "claim", None, "receipt-claim");
        assert!(!projection_replayed(&controller, "worker", &worker));
        worker_work(
            &worker,
            &step,
            action,
            Some("invented lifecycle"),
            "receipt-action",
        );
        assert!(
            !projection_replayed(&worker, "controller", &controller),
            "local {action}"
        );
        assert!(
            !projection_replayed(&controller, "worker", &worker),
            "remote {action}"
        );
        let incremental = graph_digest_of(&controller);
        assert_eq!(
            incremental,
            graph_digest_of(&worker),
            "peer parity {action}"
        );
        controller.replay_replication_graph().unwrap();
        assert_eq!(
            incremental,
            graph_digest_of(&controller),
            "canonical parity {action}"
        );
        assert!(controller.operation_projection_drift().unwrap().is_empty());
    }
}

#[test]
fn work_extensions_project_locally_and_remotely_without_full_replay() {
    let (controller, worker, step) = replicated_step_pair();
    worker_work(&worker, &step, "claim", None, "extension-claim");
    assert!(!projection_replayed(&controller, "worker", &worker));

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("observer.sqlite3");
    let observer = Store::open(&path, "observer").unwrap();
    receive_and_project(
        &observer,
        "controller",
        &exchange_from(&controller, &ReplicationInventory::default()),
    );
    let extended = worker_extend(&worker, &step);
    assert_eq!(extended.timeout_extension_ms, 300_000);
    worker_work(&worker, &step, "renew", None, "extension-quiet-renewal");
    let quiet_lease = worker
        .step_run(&step)
        .unwrap()
        .unwrap()
        .claim_expires_at_unix_ms;

    // This local extension was already applied. The next peer exchange must not turn it
    // into a full-store replay, and must preserve the local lease and execution budget.
    assert!(!projection_replayed(&worker, "controller", &controller));
    let projected = worker.step_run(&step).unwrap().unwrap();
    assert_eq!(projected.status, "claimed");
    assert_eq!(projected.claim_expires_at_unix_ms, quiet_lease);
    assert_eq!(projected.blocked_reason, extended.blocked_reason);
    let incremental = graph_digest_of(&worker);
    worker.replay_replication_graph().unwrap();
    assert_eq!(incremental, graph_digest_of(&worker));
    assert_eq!(
        worker
            .step_run(&step)
            .unwrap()
            .unwrap()
            .timeout_extension_ms,
        300_000
    );
    assert_eq!(
        worker
            .step_run(&step)
            .unwrap()
            .unwrap()
            .claim_expires_at_unix_ms,
        quiet_lease
    );

    // The remote extension arrives after a newer local state update. Rebuild only this run
    // tree in canonical order rather than overwriting that update or replaying the store.
    controller
        .set_step_state(&step, "blocked", Some("a newer update"))
        .unwrap();
    assert!(!projection_replayed(&controller, "worker", &worker));
    assert!(!projection_replayed(&observer, "controller", &controller));
    for store in [&controller, &observer] {
        let projected = store.step_run(&step).unwrap().unwrap();
        assert_eq!(projected.status, "blocked");
        assert_eq!(projected.timeout_extension_ms, 300_000);
        let incremental = graph_digest_of(store);
        store.replay_replication_graph().unwrap();
        assert_eq!(incremental, graph_digest_of(store));
        let replayed = store.step_run(&step).unwrap().unwrap();
        assert_eq!(projected.status, replayed.status);
        assert_eq!(
            projected.timeout_extension_ms,
            replayed.timeout_extension_ms
        );
        assert_eq!(projected.attempt, replayed.attempt);
    }
    let digest = graph_digest_of(&observer);
    let copy = directory.path().join("checkpoint-replay.sqlite3");
    observer.copy_store_to(&copy).unwrap();
    let mut connection = Connection::open(&copy).unwrap();
    projection_digest::register(&connection).unwrap();
    let transaction = connection.transaction().unwrap();
    checkpoint_rules::replay_from_nothing(&transaction).unwrap();
    transaction.commit().unwrap();
    assert_eq!(
        digest,
        projection_digest::root(&projection_digest::tables(&connection).unwrap())
    );
    let mut restored = step_run_row_tx(&connection, &step).unwrap().unwrap();
    enrich_step_queue(&connection, &mut restored).unwrap();
    let projected = observer.step_run(&step).unwrap().unwrap();
    assert_eq!(restored.status, projected.status);
    assert_eq!(
        restored.claim_expires_at_unix_ms,
        projected.claim_expires_at_unix_ms
    );
    assert_eq!(restored.blocked_reason, projected.blocked_reason);
    assert_eq!(
        restored.timeout_extension_ms,
        projected.timeout_extension_ms
    );
    drop(observer);
    let reopened = Store::open(&path, "observer").unwrap();
    assert_eq!(digest, graph_digest_of(&reopened));
    assert_eq!(
        reopened
            .step_run(&step)
            .unwrap()
            .unwrap()
            .timeout_extension_ms,
        300_000
    );
}

#[test]
fn work_extensions_upgrade_repairs_affected_trees_once_and_keeps_local_renewals() {
    let (controller, worker, step) = replicated_step_pair();
    worker_work(&worker, &step, "claim", None, "upgrade-extension-claim");
    let previous = worker.step_run(&step).unwrap().unwrap();
    let extended = worker_extend(&worker, &step);
    worker_work(&worker, &step, "renew", None, "upgrade-extension-quiet");
    let quiet_lease = worker
        .step_run(&step)
        .unwrap()
        .unwrap()
        .claim_expires_at_unix_ms;
    assert!(!projection_replayed(&worker, "controller", &controller));
    let corrected = graph_digest_of(&worker);

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("upgrade.sqlite3");
    worker.copy_store_to(&path).unwrap();
    {
        let connection = Connection::open(&path).unwrap();
        projection_digest::register(&connection).unwrap();
        // A pre-fix full replay left the last claim's state rather than the extension's.
        connection.execute_batch(
            "INSERT OR REPLACE INTO meta(key,value) VALUES('canonical_shared_projection_rules','2');
                 INSERT OR REPLACE INTO meta(key,value) VALUES('derived_tables_version','2');",
        ).unwrap();
        connection
            .execute(
                "UPDATE step_runs SET blocked_reason=?2, lease_expires_at_unix_ms=?3,
                    updated_at_unix_ms=?4 WHERE subject=?1",
                params![
                    step,
                    previous.blocked_reason,
                    previous
                        .claim_expires_at_unix_ms
                        .map(|value| value.to_string()),
                    previous.updated_at_unix_ms.to_string()
                ],
            )
            .unwrap();
        connection
            .execute(
                "DELETE FROM meta WHERE key='work_extended_projection_rules'",
                [],
            )
            .unwrap();
        // A full-store/operations rebuild would remove this unrelated derived row.
        connection
            .execute(
                "INSERT INTO operations(id,request_digest,canonical_claim_id,state)
                 SELECT 'op/unrelated-upgrade','digest',id,'active' FROM claims
                 WHERE kind='work.extended' LIMIT 1",
                [],
            )
            .unwrap();
    }
    let upgraded = Store::open(&path, "worker").unwrap();
    assert_eq!(
        upgraded
            .smalltalk
            .work_extension_roots_rebuilt
            .load(Ordering::Relaxed),
        1
    );
    let projected = upgraded.step_run(&step).unwrap().unwrap();
    assert_eq!(projected.status, "claimed");
    assert_eq!(projected.timeout_extension_ms, 300_000);
    assert_eq!(projected.blocked_reason, extended.blocked_reason);
    assert_eq!(projected.claim_expires_at_unix_ms, quiet_lease);
    {
        let connection = upgraded.readers.get();
        assert!(
            connection
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM operations WHERE id='op/unrelated-upgrade')",
                    [],
                    |row| row.get::<_, bool>(0),
                )
                .unwrap()
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT value FROM meta WHERE key='work_extended_projection_rules'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "1"
        );
    }
    // The operations sentinel is outside the corrected graph's real authority.
    upgraded
        .connection
        .write()
        .execute("DELETE FROM operations WHERE id='op/unrelated-upgrade'", [])
        .unwrap();
    assert_eq!(corrected, graph_digest_of(&upgraded));
    drop(upgraded);
    let reopened = Store::open(&path, "worker").unwrap();
    assert_eq!(
        reopened
            .smalltalk
            .work_extension_roots_rebuilt
            .load(Ordering::Relaxed),
        0
    );
    assert_eq!(corrected, graph_digest_of(&reopened));
    assert_eq!(
        reopened
            .step_run(&step)
            .unwrap()
            .unwrap()
            .claim_expires_at_unix_ms,
        quiet_lease
    );
}

#[test]
fn work_extensions_upgrade_and_adopt_a_v5_certificate_without_changing_its_terms() {
    use smallclaims::store::checkpoint::checkpoint_name;
    use smallclaims::store::checkpoint_agreement::{SealTerms, certificates};
    let (controller, worker, step) = replicated_step_pair();
    worker_work(&worker, &step, "claim", None, "historic-extension-claim");
    for index in 0..5 {
        controller
            .append_claim(&ClaimInput {
                subject: "daemon/controller".into(),
                kind: "daemon.diagnostic".into(),
                actor: None,
                fields: BTreeMap::from([
                    ("severity".into(), json!("warning")),
                    ("code".into(), json!("invented")),
                    ("reason".into(), json!(format!("observation {index}"))),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
    }
    assert!(!projection_replayed(&controller, "worker", &worker));
    assert!(!projection_replayed(&worker, "controller", &controller));
    let scratch = tempfile::tempdir().unwrap();
    let cut = now_ms() + 1;
    let checkpoint = checkpoint_name(cut);
    let mut legacy_digest = Sha256::new();
    legacy_digest.update(b"st3-checkpoint-rules-v1\0");
    legacy_digest.update(5_u32.to_be_bytes());
    legacy_digest.update(checkpoint_rules::rules_description().as_bytes());
    let legacy_digest = hex::encode(legacy_digest.finalize());
    assert_ne!(legacy_digest, rules_digest());
    let sealed = controller.checkpoint_sealed_identities(cut, None).unwrap();
    let terms = SealTerms {
        cut_unix_ms: cut,
        participants: BTreeSet::from(["controller".into(), "worker".into()]),
        sealed_digest: sealed.digest.clone(),
        rules_digest: legacy_digest.clone(),
    };
    for node in [&controller, &worker] {
        node.publish_seal(&checkpoint, &terms, &sealed, None)
            .unwrap();
    }
    let (_, mut plan, proof) = controller
        .plan_checkpoint_through(cut, None, scratch.path())
        .unwrap();
    assert!(proof.passed, "{proof:?}");
    assert!(!plan.claims.is_empty());
    // The sealed set contains no extensions: v5 and v6 have identical graph/timing proof
    // here. Retain the real v5 rule identity when constructing its historical certificate.
    plan.rules_digest = legacy_digest;
    for node in [&controller, &worker] {
        node.publish_verification(&checkpoint, &terms, &smallclaims::store::checkpoint_staging::StagedPlan::of(&plan), &proof)
            .unwrap();
    }
    assert!(!projection_replayed(&controller, "worker", &worker));
    assert!(!projection_replayed(&worker, "controller", &controller));
    let certificate = certificates(&worker.checkpoint_claims().unwrap(), &checkpoint);
    assert_eq!(certificate.len(), 1);

    // An extension after the historic cut survives its v5 drops and changes the live graph.
    let previous = worker.step_run(&step).unwrap().unwrap();
    let extended = worker_extend(&worker, &step);
    worker_work(&worker, &step, "renew", None, "historic-extension-quiet");
    assert!(!projection_replayed(&worker, "controller", &controller));
    let path = scratch.path().join("historic-upgrade.sqlite3");
    worker.copy_store_to(&path).unwrap();
    {
        let connection = Connection::open(&path).unwrap();
        projection_digest::register(&connection).unwrap();
        connection.execute_batch(
            "INSERT OR REPLACE INTO meta(key,value) VALUES('canonical_shared_projection_rules','2');
                 INSERT OR REPLACE INTO meta(key,value) VALUES('derived_tables_version','2');",
        ).unwrap();
        connection.execute("UPDATE step_runs SET blocked_reason=?2, updated_at_unix_ms=?3 WHERE subject=?1",
            params![step, previous.blocked_reason, previous.updated_at_unix_ms.to_string()]).unwrap();
        connection
            .execute(
                "DELETE FROM meta WHERE key='work_extended_projection_rules'",
                [],
            )
            .unwrap();
    }
    let upgraded = Store::open(&path, "worker").unwrap();
    let before = upgraded.step_run(&step).unwrap().unwrap();
    assert_eq!(before.blocked_reason, extended.blocked_reason);
    assert_eq!(before.timeout_extension_ms, 300_000);
    let graph = graph_digest_of(&upgraded);
    let reader_cut = now_ms();
    let reader =
        checkpoint_rules::subject_answers(&upgraded.readers.get(), &step, reader_cut).unwrap();
    let mut actions = Vec::new();
    controller
        .trim_checkpoint(
            &checkpoint,
            cut,
            &plan.drop_digest,
            &plan.envelopes,
            &plan.claims,
            false,
            &mut actions,
        )
        .unwrap();
    let manifest = controller.checkpoint_manifest(&checkpoint, cut).unwrap();
    assert!(!upgraded.adopt_checkpoint(&manifest).unwrap().is_empty());
    assert_eq!(graph, graph_digest_of(&upgraded));
    let after = upgraded.step_run(&step).unwrap().unwrap();
    assert_eq!(before.status, after.status);
    assert_eq!(before.blocked_reason, after.blocked_reason);
    assert_eq!(
        before.claim_expires_at_unix_ms,
        after.claim_expires_at_unix_ms
    );
    assert_eq!(before.timeout_extension_ms, after.timeout_extension_ms);
    // Use the same cut for time-dependent reader answers.
    let reader_after =
        checkpoint_rules::subject_answers(&upgraded.readers.get(), &step, reader_cut).unwrap();
    assert_eq!(reader, reader_after);
    assert_eq!(
        certificates(&upgraded.checkpoint_claims().unwrap(), &checkpoint),
        certificate
    );
    assert_eq!(
        upgraded.checkpoint_manifest(&checkpoint, cut).unwrap(),
        manifest
    );
    assert!(upgraded.adopt_checkpoint(&manifest).unwrap().is_empty());
    drop(upgraded);
    let reopened = Store::open(&path, "worker").unwrap();
    assert_eq!(
        certificates(&reopened.checkpoint_claims().unwrap(), &checkpoint),
        certificate
    );
    assert_eq!(
        reopened.checkpoint_manifest(&checkpoint, cut).unwrap(),
        manifest
    );
    assert_eq!(graph, graph_digest_of(&reopened));
}

#[test]
fn work_extensions_waiting_for_run_creation_survive_upgrade_and_later_projection() {
    let (_, worker, step) = replicated_step_pair();
    worker_work(&worker, &step, "claim", None, "delayed-extension-claim");
    let extended = worker_extend(&worker, &step);
    let extension = worker
        .claims_for(&step, Some("work.extended"))
        .unwrap()
        .remove(0);
    let sequence: u64 = worker
        .readers
        .get()
        .query_row(
            "SELECT replica_sequence FROM batches WHERE id=?1",
            [&extension.batch_id],
            |row| row.get(0),
        )
        .unwrap();
    let mut first = exchange_from(&worker, &ReplicationInventory::default());
    first
        .envelopes
        .retain(|envelope| envelope.writer == "worker" && envelope.sequence == sequence);
    assert_eq!(first.envelopes.len(), 1);
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("delayed.sqlite3");
    let target = Store::open(&path, "delayed").unwrap();
    receive_and_project(&target, "worker", &first);
    assert!(target.step_run(&step).unwrap().is_none());
    // The generation reference can arrive before the owning run as well. Knowing the
    // root's name is insufficient evidence that the extension has been projected.
    let generation = step
        .trim_start_matches("step-run/")
        .split('/')
        .next()
        .unwrap();
    let (run, revision): (String, String) = worker
        .readers
        .get()
        .query_row(
            "SELECT run_id, revision FROM run_generations WHERE id=?1",
            [generation],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    {
        let mut connection = target.connection.write();
        let transaction = connection.transaction().unwrap();
        append_claim_tx(
            &transaction,
            &target.origin,
            &format!("run-generation/{generation}"),
            "run-generation.created",
            None,
            &json!({"fields": {"run": format!("mission-run/{run}"), "revision": revision}}),
            &[],
            None,
        )
        .unwrap();
        transaction.commit().unwrap();
    }
    target
        .connection
        .write()
        .execute(
            "DELETE FROM meta WHERE key='work_extended_projection_rules'",
            [],
        )
        .unwrap();
    drop(target);
    let target = Store::open(&path, "delayed").unwrap();
    assert!(
        !target
            .readers
            .get()
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM meta WHERE key='work_extended_projection_rules')",
                [],
                |row| row.get::<_, bool>(0),
            )
            .unwrap(),
        "an unresolved extension must not be marked migrated"
    );
    // An older store that still needs the canonical full replay must leave the same
    // migration pending until the owning run arrives.
    target
        .connection
        .write()
        .execute(
            "DELETE FROM meta WHERE key='canonical_shared_projection_rules'",
            [],
        )
        .unwrap();
    drop(target);
    let target = Store::open(&path, "delayed").unwrap();
    assert!(
        !target
            .readers
            .get()
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM meta WHERE key='work_extended_projection_rules')",
                [],
                |row| row.get::<_, bool>(0),
            )
            .unwrap()
    );
    assert!(!projection_replayed(&target, "worker", &worker));
    let projected = target.step_run(&step).unwrap().unwrap();
    assert_eq!(
        projected.timeout_extension_ms,
        extended.timeout_extension_ms
    );
    assert_eq!(projected.blocked_reason, extended.blocked_reason);
    assert_eq!(
        projected.claim_expires_at_unix_ms,
        extended.claim_expires_at_unix_ms
    );
    let digest = graph_digest_of(&target);
    target.replay_replication_graph().unwrap();
    assert_eq!(digest, graph_digest_of(&target));
    drop(target);
    let target = Store::open(&path, "delayed").unwrap();
    assert_eq!(
        target
            .readers
            .get()
            .query_row(
                "SELECT value FROM meta WHERE key='work_extended_projection_rules'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
        "1"
    );
    assert_eq!(digest, graph_digest_of(&target));
}

#[test]
fn work_extensions_sync_authority_with_previous_projection_rules_without_comparing_graphs() {
    let (controller, worker, step) = replicated_step_pair();
    worker_work(&worker, &step, "claim", None, "mixed-extension-claim");
    worker_extend(&worker, &step);
    let legacy = canonical_hash(&(
        "st3.shared-projections.resources.v1",
        st3_schema::registry().digest(),
    ))
    .unwrap();
    let mut exchange = exchange_from(&worker, &controller.replication_inventory().unwrap());
    assert_ne!(exchange.schema_digest, legacy);
    exchange.schema_digest = legacy.clone();
    exchange.projection_registry_digest = None;
    exchange.projection_versions.clear();
    controller
        .receive_replication_exchange_asking("worker", TEST_FLEET, &exchange, true)
        .unwrap();
    let admitted = controller.validate_replication_backlog().unwrap();
    assert_eq!(admitted.unknown, 0);
    assert_eq!(admitted.invalid, 0);
    assert!(controller.project_replication_backlog().unwrap());
    assert_eq!(
        controller
            .step_run(&step)
            .unwrap()
            .unwrap()
            .timeout_extension_ms,
        300_000
    );
    let mut summary = worker.export_replication_summary(TEST_FLEET).unwrap();
    summary.schema_digest = legacy;
    summary.projection_registry_digest = None;
    summary.projection_versions.clear();
    summary.graph_digest = "invented-previous-layout-graph".into();
    summary
        .projection_digests
        .insert("step_runs".into(), "invented-previous-layout-table".into());
    controller
        .receive_replication_exchange_asking("worker", TEST_FLEET, &summary, true)
        .unwrap();
    let status = controller
        .replication_status(true, Some(TEST_FLEET), &["worker".into()])
        .unwrap();
    assert!(status.peers[0].projection_comparison_waiting);
    assert!(status.peers[0].differing_tables.is_empty());
    assert!(!status.peers[0].sync.as_ref().unwrap().diverged);
    assert_eq!(status.unhealthy_projections, 0);
    assert_eq!(
        status.authority_digest,
        worker
            .replication_status(true, Some(TEST_FLEET), &[])
            .unwrap()
            .authority_digest
    );
}

#[test]
fn quiet_renewals_preserve_the_shared_step_reason_and_projection_digest() {
    for working in [false, true] {
        let (controller, worker, step) = replicated_step_pair();
        let request = |reason: Option<&str>, key: &str| WorkRequest {
            wait_ms: None,
            actor: Some("agent/worker.one".into()),
            incarnation: Some("worker-generation".into()),
            summary: None, reason: reason.map(str::to_owned), evidence: Vec::new(),
            idempotency_key: key.into(),
        };
        worker.work_action(&step, "claim", &request(Some("durable claim reason"), "reason-claim")).unwrap();
        if working {
            worker.work_action(&step, "progress", &request(Some("durable progress reason"), "reason-progress")).unwrap();
        }
        receive_and_project(&controller, "worker", &exchange_from(&worker, &controller.replication_inventory().unwrap()));
        let before = worker.step_run(&step).unwrap().unwrap();
        let snapshot = worker.replication_snapshot().unwrap();
        let claims_before = worker.claims_for(&step, Some("work.renewed")).unwrap().len();
        // Use the ordinary clock and the existing renewal cadence: this stays inside
        // the five-minute replicated anchor margin and changes no production deadline.
        std::thread::sleep(std::time::Duration::from_millis(2));
        let renewed = worker.work_action(&step, "renew", &request(None, "reason-quiet-renew")).unwrap();
        assert!(renewed.claim_expires_at_unix_ms > before.claim_expires_at_unix_ms);
        assert_eq!(renewed.blocked_reason, before.blocked_reason);
        assert_eq!(worker.claims_for(&step, Some("work.renewed")).unwrap().len(), claims_before);
        let after = worker.replication_snapshot().unwrap();
        assert_eq!(after.authority_digest, snapshot.authority_digest);
        assert_eq!(after.projection_digests, snapshot.projection_digests);
        assert_eq!(after.projection_digests, controller.replication_snapshot().unwrap().projection_digests);
        worker.replay_replication_graph().unwrap();
        let replayed = worker.step_run(&step).unwrap().unwrap();
        assert_eq!(replayed.blocked_reason, before.blocked_reason);
        assert_eq!(replayed.claim_expires_at_unix_ms, renewed.claim_expires_at_unix_ms);
        assert_eq!(worker.replication_snapshot().unwrap().projection_digests, after.projection_digests);
        // A published renewal still updates the shared reason through its durable claim.
        let published = worker.work_action(&step, "renew", &request(Some("new durable reason"), "reason-published-renew")).unwrap();
        assert_eq!(published.blocked_reason.as_deref(), Some("new durable reason"));
        assert_eq!(worker.claims_for(&step, Some("work.renewed")).unwrap().len(), claims_before + 1);
        receive_and_project(&controller, "worker", &exchange_from(&worker, &controller.replication_inventory().unwrap()));
        assert_eq!(controller.step_run(&step).unwrap().unwrap().blocked_reason, published.blocked_reason);
        assert_eq!(worker.replication_snapshot().unwrap().projection_digests, controller.replication_snapshot().unwrap().projection_digests);
    }
}

/// A seat's own lease renewals, and the lane, runtime-action and intake claims its node writes,
/// were projected when they were written. Every one of them used to make the next peer
/// exchange replay the whole graph while holding the store's only writer. The exchange now
/// extends the graph, and the graph is the one a replay from nothing produces.
#[test]
fn local_renewals_and_event_claims_extend_the_graph_without_a_replay() {
    let (controller, worker, step) = replicated_step_pair();
    worker_work(&worker, &step, "claim", None, "lease-claim");
    std::thread::sleep(std::time::Duration::from_millis(2));
    worker_work(
        &worker,
        &step,
        "renew",
        Some("still working"),
        "lease-renew",
    );
    std::thread::sleep(std::time::Duration::from_millis(2));
    // A quiet renewal moves only the local lease and writes no claim.
    worker_work(&worker, &step, "renew", None, "lease-quiet-renew");
    {
        let mut connection = worker.connection.lock().unwrap();
        let transaction = connection.transaction().unwrap();
        for (subject, kind, fields, operation) in [
            (
                "lane/fleet/test/train",
                "lane.marked",
                json!({"entry": "mission-run/test-entry", "state": "waiting"}),
                Some("op/lane-mark"),
            ),
            (
                "agent/worker.one",
                "runtime.action.requested",
                json!({"action": "terminate"}),
                Some("op/action"),
            ),
            (
                "subscription/test/intake",
                "subscription.mission-requested",
                json!({
                    "mission": "mission/test", "mission_revision": "revision",
                    "resource": "resource/test", "resource_input": "source",
                    "workspace": "/tmp/test", "discovery": "discovery-pending"
                }),
                None,
            ),
        ] {
            let mut body = json!({"fields": fields});
            if let Some(operation) = operation {
                body["_operation"] = json!({"id": operation, "request_digest": "digest"});
            }
            append_claim_tx(
                &transaction,
                &worker.origin,
                subject,
                kind,
                Some("agent/worker.one"),
                &body,
                &[],
                None,
            )
            .unwrap();
        }
        transaction.commit().unwrap();
    }
    controller
        .append_claim(&ClaimInput {
            subject: "agent/controller.watch".into(),
            kind: "harness.observed".into(),
            actor: Some("agent/controller.watch".into()),
            fields: serde_json::from_value(json!({"state": "idle"})).unwrap(),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some("controller-heartbeat".into()),
        })
        .unwrap();

    assert!(!projection_replayed(&worker, "controller", &controller));
    let step_run = worker.step_run(&step).unwrap().unwrap();
    assert_eq!(step_run.claimant.as_deref(), Some("agent/worker.one"));
    let incremental = graph_digest_of(&worker);
    worker.replay_replication_graph().unwrap();
    assert_eq!(incremental, graph_digest_of(&worker));
}

/// Another writer's structural claim that sorts before a lease claim this node already
/// applied rebuilds that step's run tree, without replaying the graph, and the step ends as
/// the replay's order decides.
#[test]
fn catch_up_projection_preserves_out_of_order_work_and_local_progress_between_chunks() {
    let (controller, worker, step) = replicated_step_pair();
    let accepted = worker
        .claims_for(&step, None)
        .unwrap()
        .iter()
        .map(|claim| claim.accepted_at_unix_ms)
        .max()
        .unwrap()
        + 10;
    controller.set_write_clock_at(accepted).unwrap();
    for index in 0..300 {
        controller
            .append_claim(&ClaimInput {
                subject: format!("custom/work-catch-up/{index}"),
                kind: "custom.work-catch-up.note".into(),
                actor: None,
                fields: BTreeMap::new(),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        if index == 100 {
            controller
                .set_step_state(&step, "blocked", Some("an older update"))
                .unwrap();
        }
        if index == 200 {
            controller
                .set_step_state(&step, "ready", Some("the older block ends"))
                .unwrap();
        }
    }
    worker.set_write_clock_at(accepted + 10).unwrap();
    worker_work(&worker, &step, "claim", None, "catch-up-local-claim");
    let exchange = exchange_from(&controller, &worker.replication_inventory().unwrap());
    worker
        .receive_replication_exchange("controller", TEST_FLEET, &exchange)
        .unwrap();
    worker.validate_replication_backlog().unwrap();
    let mut writes = 0;
    FULL_REPLAYS.with(|count| count.set(0));
    assert!(
        worker
            .project_replication_backlog_with_yield(|| {
                writes += 1;
                worker.set_write_clock_at(accepted + 10 + writes).unwrap();
                worker_work(
                    &worker,
                    &step,
                    "progress",
                    Some("working during catch-up"),
                    &format!("catch-up-local-progress-{writes}"),
                );
                assert_eq!(worker.step_run(&step).unwrap().unwrap().status, "working");
            })
            .unwrap()
    );
    assert!(
        writes >= 2,
        "normal work updates need turns between projection chunks"
    );
    worker.project_replication_backlog().unwrap();
    assert_eq!(FULL_REPLAYS.with(std::cell::Cell::get), 0);
    let incremental = graph_digest_of(&worker);
    let before = worker.step_run(&step).unwrap().unwrap();
    worker.replay_replication_graph().unwrap();
    let after = worker.step_run(&step).unwrap().unwrap();
    assert_eq!(
        (&before.status, &before.claimant, &before.blocked_reason),
        (&after.status, &after.claimant, &after.blocked_reason)
    );
    assert_eq!(incremental, graph_digest_of(&worker));
}

#[test]
fn a_run_tree_with_only_this_nodes_new_claims_is_not_rebuilt_by_a_projection_pass() {
    let (_controller, worker, step) = replicated_step_pair();
    worker_work(&worker, &step, "claim", None, "local-only-claim");
    worker_work(&worker, &step, "progress", Some("working"), "local-only-progress");
    RUN_TREE_REBUILDS.with(|rebuilds| rebuilds.set(0));
    FULL_REPLAYS.with(|replays| replays.set(0));
    worker.project_replication_backlog().unwrap();
    // The claims were applied when written; a pass that sees nothing from another writer for
    // the tree has nothing to reconcile, and must not re-derive the whole tree for them.
    assert_eq!(RUN_TREE_REBUILDS.with(std::cell::Cell::get), 0);
    assert_eq!(FULL_REPLAYS.with(std::cell::Cell::get), 0);
    let incremental = graph_digest_of(&worker);
    let before = worker.step_run(&step).unwrap().unwrap();
    worker.replay_replication_graph().unwrap();
    let after = worker.step_run(&step).unwrap().unwrap();
    assert_eq!(
        (&before.status, &before.claimant, &before.blocked_reason),
        (&after.status, &after.claimant, &after.blocked_reason)
    );
    assert_eq!(incremental, graph_digest_of(&worker));
}

#[test]
fn a_local_claim_sorting_before_a_projected_peer_claim_still_rebuilds_its_run_tree() {
    let (controller, worker, step) = replicated_step_pair();
    // The peer's clock runs ahead: its claim is dated ten minutes from now.
    controller
        .set_write_clock_at(now_ms() + 600_000)
        .unwrap();
    controller
        .set_step_state(&step, "blocked", Some("the peer blocks it"))
        .unwrap();
    assert!(!projection_replayed(&worker, "controller", &controller));
    assert_eq!(worker.step_run(&step).unwrap().unwrap().status, "blocked");
    // The worker then writes by its own, earlier clock: its claim sorts before the peer's, so
    // a replay ends with the step blocked while the write path left it ready. The pass that
    // sees only this local claim must still rebuild the tree to agree with the replay.
    worker.set_write_clock_at(now_ms()).unwrap();
    worker
        .set_step_state(&step, "ready", Some("the worker unblocks it"))
        .unwrap();
    RUN_TREE_REBUILDS.with(|rebuilds| rebuilds.set(0));
    worker.project_replication_backlog().unwrap();
    assert!(
        RUN_TREE_REBUILDS.with(std::cell::Cell::get) >= 1,
        "a local claim that sorts before a projected peer claim marks its tree dirty"
    );
    let incremental = graph_digest_of(&worker);
    let projected = worker.step_run(&step).unwrap().unwrap().status;
    worker.replay_replication_graph().unwrap();
    assert_eq!(incremental, graph_digest_of(&worker));
    assert_eq!(projected, worker.step_run(&step).unwrap().unwrap().status);
}

#[test]
fn a_peer_claim_older_than_a_local_lease_claim_rebuilds_its_run_tree() {
    let (controller, worker, step) = replicated_step_pair();
    controller
        .set_step_state(&step, "cancelled", Some("the controller cancels it"))
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(2));
    worker_work(&worker, &step, "claim", None, "late-claim");

    assert!(!projection_replayed(&worker, "controller", &controller));
    assert!(!projection_replayed(&controller, "worker", &worker));
    assert_eq!(graph_digest_of(&worker), graph_digest_of(&controller));
    let incremental = graph_digest_of(&worker);
    worker.replay_replication_graph().unwrap();
    assert_eq!(incremental, graph_digest_of(&worker));
}

#[test]
/// Rebuilding one run tree from its own claims gives what the write path wrote and what a
/// replay from nothing gives.
fn a_rebuilt_run_tree_matches_the_write_path_and_a_replay() {
    let (_controller, worker, step) = replicated_step_pair();
    worker
        .set_step_state(&step, "ready", Some("a seat can take it"))
        .unwrap();
    worker_work(&worker, &step, "claim", None, "debug-claim");
    let written = worker.step_run(&step).unwrap().unwrap();
    {
        let mut connection = worker.connection.lock().unwrap();
        let transaction = connection.transaction().unwrap();
        let root = run_tree_of_tx(&transaction, &step).unwrap().unwrap();
        rebuild_run_tree_tx(&transaction, &root).unwrap();
        transaction.commit().unwrap();
    }
    let rebuilt = worker.step_run(&step).unwrap().unwrap();
    worker.replay_replication_graph().unwrap();
    let replayed = worker.step_run(&step).unwrap().unwrap();
    assert_eq!(
        (&written.status, &written.claimant),
        (&rebuilt.status, &rebuilt.claimant)
    );
    assert_eq!(
        (&rebuilt.status, &rebuilt.claimant),
        (&replayed.status, &replayed.claimant)
    );
}

#[test]
/// A run that adopted a new revision keeps its new generation when its tree is rebuilt.
fn a_rebuilt_run_tree_keeps_an_adopted_revision() {
    let store = Store::open_memory("node").unwrap();
    let source = |revision: usize| {
        format!(
            "version 2\nmission \"adopt\" state=\"ready\" {{\n  goal \"Revision {revision}.\"\n  step \"prepare\" {{ }}\n  step \"check\" {{ depends-on {{ step \"prepare\" completed }} }}\n}}\n"
        )
    };
    let publish = |revision: usize| {
        let intent = crate::graph::parse_test_intent(&source(revision), "node").unwrap();
        let planned = store
            .mission(
                &intent,
                IntentInput {
                    kdl: source(revision),
                    source_name: None,
                },
            )
            .unwrap();
        store
            .apply(
                &intent,
                &planned.subject_tokens,
                &format!("adopt-{revision}"),
            )
            .unwrap();
        intent
    };
    publish(1);
    let run = store
        .create_mission_run(&MissionRunRequest {
            mission: "adopt".into(),
            revision: None,
            workspace: "/tmp".into(),
            requester: Some("person/test".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "adopt-run".into(),
        })
        .unwrap();
    let intent = publish(2);
    store
        .adopt_mission_revision(
            &run.id,
            &intent.missions["adopt"],
            "person/test",
            "reopen it",
            "adopt-key",
        )
        .unwrap();
    let written = store.mission_run(&run.id).unwrap().unwrap();
    {
        let mut connection = store.connection.lock().unwrap();
        let transaction = connection.transaction().unwrap();
        let root = run_tree_of_tx(&transaction, &run.subject).unwrap().unwrap();
        rebuild_run_tree_tx(&transaction, &root).unwrap();
        transaction.commit().unwrap();
    }
    let rebuilt = store.mission_run(&run.id).unwrap().unwrap();
    store.replay_replication_graph().unwrap();
    let replayed = store.mission_run(&run.id).unwrap().unwrap();
    assert_eq!(written.generation, rebuilt.generation);
    assert_eq!(
        (&rebuilt.generation, &rebuilt.status),
        (&replayed.generation, &replayed.status)
    );
}

/// A standing root with `count` child runs started from its step, replicated from a
/// controller to a worker. Returns the stores, the root and the children.
pub(super) fn replicated_run_tree(
    count: usize,
) -> (Store, Store, MissionRunView, Vec<MissionRunView>) {
    let controller = Store::open_memory("controller").unwrap();
    let kdl = r#"
version 2

  mission "front-desk" state="ready" {
    goal "Take requests."
    step "take" { assigned-to "agent/desk.one" }
  }

  mission "request" state="ready" {
    goal "Answer one request."
    concurrent-runs
    step "answer" { assigned-to "agent/worker.one" }
  }

"#;
    let intent = parse_intent(kdl, "controller").unwrap();
    let planned = controller
        .mission(&intent, IntentInput { kdl: kdl.into(), source_name: None })
        .unwrap();
    controller
        .apply(&intent, &planned.subject_tokens, "run-tree-missions")
        .unwrap();
    let request = |mission: &str, key: String| MissionRunRequest {
        mission: mission.into(),
        revision: None,
        workspace: "/tmp".into(),
        requester: Some("person/test".into()),
        mode: Some("run".into()),
        inputs: BTreeMap::new(),
        idempotency_key: key,
    };
    let root = controller
        .create_mission_run(&request("front-desk", "front-desk-run".into()))
        .unwrap();
    let take = root.steps[0].subject.clone();
    let children = (0..count)
        .map(|index| {
            controller
                .create_child_mission_run(
                    &request("request", format!("request-{index}")),
                    &root,
                    &take,
                    None,
                )
                .unwrap()
        })
        .collect::<Vec<_>>();
    for child in &children {
        controller.set_step_state(&child.steps[0].subject, "ready", None).unwrap();
    }
    let worker = Store::open_memory("worker").unwrap();
    replicate_fully(&controller, &worker);
    (controller, worker, root, children)
}

/// Exchange from `controller` to `worker` until the worker holds every claim: an export
/// sends one bounded page, so a large tree takes several.
fn replicate_fully(controller: &Store, worker: &Store) {
    for _ in 0..64 {
        let exchange = exchange_from(controller, &worker.replication_inventory().unwrap());
        if exchange.envelopes.is_empty() {
            break;
        }
        receive_and_project(worker, "controller", &exchange);
    }
    assert_eq!(
        worker.replication_inventory().unwrap().digest,
        controller.replication_inventory().unwrap().digest,
        "the worker caught up"
    );
}

/// One late claim about one child of a standing root rebuilds that child alone, not the
/// root's thousands of other children, and ends where a replay of the whole graph ends.
#[test]
fn a_late_claim_in_a_standing_tree_rebuilds_only_its_own_child() {
    let (controller, worker, _root, children) = replicated_run_tree(8);
    let late = children[3].steps[0].subject.clone();
    let earlier = worker
        .claims_for(&children[0].subject, None)
        .unwrap()
        .iter()
        .map(|claim| claim.accepted_at_unix_ms)
        .min()
        .unwrap();
    // The worker projects a later claim about another child first.
    worker
        .set_step_state(&children[7].steps[0].subject, "blocked", Some("waits on a person"))
        .unwrap();
    controller.set_write_clock_at(earlier).unwrap();
    controller
        .set_step_state(&late, "blocked", Some("the controller blocks it early"))
        .unwrap();
    RUN_TREE_REBUILDS.with(|rebuilds| rebuilds.set(0));
    RUN_PART_RUNS.with(|runs| runs.set(0));
    assert!(!projection_replayed(&worker, "controller", &controller));
    assert!(RUN_TREE_REBUILDS.with(std::cell::Cell::get) >= 1, "the late claim rebuilds");
    assert_eq!(
        RUN_PART_RUNS.with(std::cell::Cell::get),
        RUN_TREE_REBUILDS.with(std::cell::Cell::get),
        "each rebuild holds the one child the late claim is about"
    );
    let incremental = graph_digest_of(&worker);
    let projected = worker.step_run(&late).unwrap().unwrap();
    worker.replay_replication_graph().unwrap();
    assert_eq!(incremental, graph_digest_of(&worker));
    let replayed = worker.step_run(&late).unwrap().unwrap();
    assert_eq!(
        (&projected.status, &projected.blocked_reason),
        (&replayed.status, &replayed.blocked_reason)
    );
}

/// Once a claim ends the root, its children are no longer independent of it: a late claim
/// rebuilds the whole tree, as a replay orders the root's end among them.
#[test]
fn a_late_claim_in_a_tree_whose_root_ended_rebuilds_the_whole_tree() {
    let (controller, worker, root, children) = replicated_run_tree(8);
    let earlier = worker
        .claims_for(&children[0].subject, None)
        .unwrap()
        .iter()
        .map(|claim| claim.accepted_at_unix_ms)
        .min()
        .unwrap();
    worker
        .set_mission_run_state(&root.id, "cancelled", "terminal", Some("the desk closes"))
        .unwrap();
    controller.set_write_clock_at(earlier).unwrap();
    controller
        .set_step_state(&children[3].steps[0].subject, "blocked", Some("blocked early"))
        .unwrap();
    RUN_TREE_REBUILDS.with(|rebuilds| rebuilds.set(0));
    RUN_PART_RUNS.with(|runs| runs.set(0));
    assert!(!projection_replayed(&worker, "controller", &controller));
    assert!(RUN_TREE_REBUILDS.with(std::cell::Cell::get) >= 1);
    assert_eq!(RUN_PART_RUNS.with(std::cell::Cell::get), 0, "no part rebuilds alone");
    let incremental = graph_digest_of(&worker);
    worker.replay_replication_graph().unwrap();
    assert_eq!(incremental, graph_digest_of(&worker));
}

/// Every part of a standing tree, rebuilt alone, leaves the graph a replay leaves.
#[test]
fn each_part_of_a_standing_tree_rebuilt_alone_matches_a_replay() {
    let (_controller, worker, root, children) = replicated_run_tree(8);
    worker_work(&worker, &children[0].steps[0].subject, "claim", None, "part-claim");
    worker
        .set_step_state(&children[1].steps[0].subject, "blocked", Some("waits on a person"))
        .unwrap();
    worker.replay_replication_graph().unwrap();
    let replayed = graph_digest_of(&worker);
    let mut connection = worker.connection.lock().unwrap();
    let transaction = connection.transaction().unwrap();
    let mut parts = BTreeSet::new();
    for subject in std::iter::once(root.subject.clone())
        .chain(children.iter().map(|child| child.steps[0].subject.clone()))
    {
        parts.insert(run_part_of_tx(&transaction, &subject).unwrap().unwrap());
    }
    assert_eq!(parts.len(), 1 + children.len(), "{parts:?}");
    assert!(parts.contains(&Aggregate::RunPart(root.id.clone())));
    RUN_PART_RUNS.with(|runs| runs.set(0));
    for part in &parts {
        let Aggregate::RunPart(top) = part else { panic!("{part:?}") };
        rebuild_run_part_tx(&transaction, top).unwrap();
    }
    assert_eq!(RUN_PART_RUNS.with(std::cell::Cell::get), 1 + children.len());
    assert_eq!(graph_digest(&transaction).unwrap(), replayed);
    transaction.rollback().unwrap();
}

/// The shapes a standing tree holds besides children started from the root's steps, made on
/// the controller and replicated to the worker: a grandchild started from a child's step, a
/// run a subscription delivers straight under the root, and a person ask owned by the root
/// and one owned by a child.
struct TreeShapes {
    controller: Store,
    worker: Store,
    root: MissionRunView,
    children: Vec<MissionRunView>,
    grandchild: MissionRunView,
    delivered: MissionRunView,
    root_ask: StepRunView,
    child_ask: StepRunView,
}

fn tree_shapes() -> TreeShapes {
    let (controller, worker, root, children) = replicated_run_tree(4);
    // Only a declared agent's live work can ask a person.
    let agents = crate::graph::parse_internal_intent(
        r#"version 2
agent "desk.one" { workspace "/tmp"; command "true"; restart always; }
agent "worker.one" { workspace "/tmp"; command "true"; restart always; }
"#,
        "controller",
    )
    .unwrap();
    controller.apply_internal(&agents, "tree-shape-agents").unwrap();
    let request = |key: &str| MissionRunRequest {
        mission: "request".into(),
        revision: None,
        workspace: "/tmp".into(),
        requester: Some("person/test".into()),
        mode: Some("run".into()),
        inputs: BTreeMap::new(),
        idempotency_key: key.into(),
    };
    let grandchild = controller
        .create_child_mission_run(
            &request("grandchild"),
            &children[1],
            &children[1].steps[0].subject,
            None,
        )
        .unwrap();
    let delivered = controller
        .create_child_mission_run(
            &request("delivered"),
            &root,
            "subscription/front-desk-intake",
            None,
        )
        .unwrap();
    for run in [&root, &grandchild, &delivered] {
        controller.set_step_state(&run.steps[0].subject, "ready", None).unwrap();
    }
    let ask = |step: &str, actor: &str, key: &str| {
        controller
            .work_action(
                step,
                "claim",
                &WorkRequest {
                    wait_ms: None,
                    actor: Some(actor.into()),
                    incarnation: Some(format!("{key}-incarnation")),
                    summary: None,
                    reason: None,
                    evidence: Vec::new(),
                    idempotency_key: format!("{key}-claim"),
                },
            )
            .unwrap();
        controller
            .ask_person(&crate::model::PersonAskRequest {
                legacy_request: None,
                person: "person/test".into(),
                title: "Choose a date".into(),
                reason: "Reply with a date.".into(),
                actor: actor.into(),
                step: Some(step.into()),
                new_run: None,
                incarnation: Some(format!("{key}-incarnation")),
                request: None,
                idempotency_key: key.into(),
            })
            .unwrap()
    };
    let root_ask = ask(&root.steps[0].subject, "agent/desk.one", "root-ask");
    // The worker's seat queue takes its oldest ready step first.
    let child_ask = ask(&children[0].steps[0].subject, "agent/worker.one", "child-ask");
    replicate_fully(&controller, &worker);
    TreeShapes {
        controller,
        worker,
        root,
        children,
        grandchild,
        delivered,
        root_ask,
        child_ask,
    }
}

/// The worker projects a later claim about one child; `late` then writes claims on the
/// controller dated before it. Returns how many runs the projection rebuilt as parts, after
/// checking the graph against a full replay.
fn late_claim_matches_a_replay(shapes: &TreeShapes, late: impl FnOnce(&Store)) -> usize {
    let now = now_ms();
    shapes.worker.set_write_clock_at(now + 2_000).unwrap();
    shapes
        .worker
        .set_step_state(
            &shapes.children[3].steps[0].subject,
            "blocked",
            Some("waits on a person"),
        )
        .unwrap();
    shapes.controller.set_write_clock_at(now + 1_000).unwrap();
    late(&shapes.controller);
    RUN_PART_RUNS.with(|runs| runs.set(0));
    assert!(!projection_replayed(&shapes.worker, "controller", &shapes.controller));
    let parts = RUN_PART_RUNS.with(std::cell::Cell::get);
    let incremental = graph_digest_of(&shapes.worker);
    shapes.worker.replay_replication_graph().unwrap();
    assert_eq!(incremental, graph_digest_of(&shapes.worker), "the projection matches a replay");
    parts
}

/// The tree's runs: the root, its children, the grandchild, the delivered run and two asks.
const TREE_SHAPE_RUNS: usize = 1 + 4 + 1 + 1 + 2;

#[test]
fn a_late_claim_about_a_grandchild_rebuilds_its_child_part_and_matches_a_replay() {
    let shapes = tree_shapes();
    let step = shapes.grandchild.steps[0].subject.clone();
    let parts = late_claim_matches_a_replay(&shapes, |controller| {
        controller.set_step_state(&step, "blocked", Some("blocked early")).unwrap();
    });
    assert!((1..TREE_SHAPE_RUNS).contains(&parts), "{parts} runs rebuilt as parts");
}

#[test]
fn a_late_claim_about_a_delivered_run_rebuilds_its_part_and_matches_a_replay() {
    let shapes = tree_shapes();
    let step = shapes.delivered.steps[0].subject.clone();
    let parts = late_claim_matches_a_replay(&shapes, |controller| {
        controller.set_step_state(&step, "blocked", Some("blocked early")).unwrap();
    });
    assert!((1..TREE_SHAPE_RUNS).contains(&parts), "{parts} runs rebuilt as parts");
}

#[test]
fn late_claims_about_asks_owned_by_the_root_and_by_a_child_match_a_replay() {
    for owned_by_root in [true, false] {
        let shapes = tree_shapes();
        let ask = if owned_by_root { &shapes.root_ask } else { &shapes.child_ask };
        let ask = ask.subject.clone();
        let parts = late_claim_matches_a_replay(&shapes, |controller| {
            controller.set_step_state(&ask, "blocked", Some("blocked early")).unwrap();
        });
        assert!(
            (1..TREE_SHAPE_RUNS).contains(&parts),
            "{parts} runs rebuilt as parts (owned by the root: {owned_by_root})"
        );
    }
}

#[test]
fn a_late_claim_in_the_batch_that_ends_the_root_matches_a_replay() {
    let shapes = tree_shapes();
    let step = shapes.children[2].steps[0].subject.clone();
    let root = shapes.root.id.clone();
    late_claim_matches_a_replay(&shapes, |controller| {
        controller.set_step_state(&step, "blocked", Some("blocked early")).unwrap();
        controller
            .set_mission_run_state(&root, "cancelled", "terminal", Some("the desk closes"))
            .unwrap();
    });
}

#[test]
fn lanes_runtime_actions_and_intake_extend_the_graph_incrementally() {
    for kind in [
        "lane.marked",
        "lane.joined",
        "runtime.action.requested",
        "runtime.action.succeeded",
        "subscription.mission-requested",
        "subscription.mission-started",
        "observer.refresh-requested",
        "publication.operation",
    ] {
        assert!(Store::simple_replication_kind(kind), "{kind}");
    }
    // Projections read loop and attention claims, or rebuild from checkpoints and repairs.
    for kind in [
        "planning-session.started",
        "loop.round-result",
        "attention.resolved",
        "checkpoint.sealed",
        "record.repaired",
    ] {
        assert!(!Store::simple_replication_kind(kind), "{kind}");
    }
}

#[test]
fn a_run_started_with_a_reporter_records_it_and_an_unset_run_records_nothing() {
    let store = Store::open_memory("node").unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let plain = publish_mission(
        &store,
        "version 2\nmission \"plain\" state=\"ready\" { concurrent-runs; goal \"Plain.\"; step \"work\" { agentless } }",
        "publish-plain",
    );
    let watched = publish_mission(
        &store,
        "version 2\nmission \"watched\" state=\"ready\" report-to=\"agent/ops/mission-reporter\" stalled-after=\"15m\" { concurrent-runs; goal \"Watched.\"; step \"work\" { agentless } }",
        "publish-watched",
    );
    let declare = |run: &str, mission: &MissionSpec, extra: &str| {
        let source = format!(
            "version 2\nmission-run {run:?} {{\n  mission \"mission/{}@{}\"\n  workspace {:?}\n  requester \"person/operator\"\n{extra}}}\n",
            mission.id,
            mission.revision,
            workspace.path().display().to_string(),
        );
        let intent = crate::graph::parse_intent(&source, "node").unwrap();
        let preview = store
            .mission(
                &intent,
                IntentInput {
                    kdl: source,
                    source_name: None,
                },
            )
            .unwrap();
        (intent, preview)
    };
    let start = |run: &str, mission: &MissionSpec, extra: &str| {
        let (intent, preview) = declare(run, mission, extra);
        assert!(preview.blockers.is_empty(), "{:?}", preview.blockers);
        store
            .apply_as(
                &intent,
                &preview.subject_tokens,
                &format!("start-{run}"),
                Some("person/operator"),
            )
            .unwrap();
        store
            .latest_claim(&format!("mission-run/{run}"), Some("mission-run.created"))
            .unwrap()
            .unwrap()
            .body["fields"]
            .clone()
    };

    let unset = start("plain/1", &plain, "");
    for field in ["report_to", "stalled_after_ms", "report_completed"] {
        assert!(unset.get(field).is_none(), "{field}");
    }
    // Whoever starts a run names the reporter, and the default limit applies.
    let named = start(
        "plain/2",
        &plain,
        "  report-to \"agent/ops/starter\"\n  report-completed \"true\"\n",
    );
    assert_eq!(named["report_to"], "agent/ops/starter");
    assert_eq!(named["stalled_after_ms"], 30 * 60_000);
    assert_eq!(named["report_completed"], true);
    // The mission's own reporter and limit apply when the start names none.
    let inherited = start("watched/1", &watched, "");
    assert_eq!(inherited["report_to"], "agent/ops/mission-reporter");
    assert_eq!(inherited["stalled_after_ms"], 15 * 60_000);
    assert!(inherited.get("report_completed").is_none());
    // The start's reporter replaces the mission's.
    let replaced = start("watched/2", &watched, "  report-to \"agent/ops/starter\"\n");
    assert_eq!(replaced["report_to"], "agent/ops/starter");
    assert_eq!(replaced["stalled_after_ms"], 15 * 60_000);

    // Declaring the same run again with different reporting is not a retry: the stored
    // creation claim would be left unchanged while the operator is told it was accepted.
    let different = "mission run `mission-run/plain/2` already exists with different creation fields";
    for (what, extra) in [
        ("another reporter", "  report-to \"agent/ops/other\"\n  report-completed \"true\"\n"),
        ("no reporting at all", ""),
        ("the reporter alone, dropping completion", "  report-to \"agent/ops/starter\"\n"),
    ] {
        let (_, changed) = declare("plain/2", &plain, extra);
        assert_eq!(changed.blockers, [different], "{what}");
    }
    // Adding completion to a run that was started without it is not a retry either.
    let (_, added) = declare(
        "watched/2",
        &watched,
        "  report-to \"agent/ops/starter\"\n  report-completed \"true\"\n",
    );
    assert_eq!(
        added.blockers,
        ["mission run `mission-run/watched/2` already exists with different creation fields"]
    );
    // The exact declarations, and one that leaves the mission's own reporter to apply, are.
    for (run, mission, extra) in [
        (
            "plain/2",
            &plain,
            "  report-to \"agent/ops/starter\"\n  report-completed \"true\"\n",
        ),
        ("watched/1", &watched, ""),
        ("watched/2", &watched, "  report-to \"agent/ops/starter\"\n"),
        ("plain/1", &plain, ""),
    ] {
        let (_, same) = declare(run, mission, extra);
        assert!(same.blockers.is_empty(), "{run}: {:?}", same.blockers);
    }
}

#[test]
fn a_running_runs_reporter_changes_by_its_own_claim_and_leaves_creation_alone() {
    let store = Store::open_memory("node").unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let watched = publish_mission(
        &store,
        "version 2\nmission \"watched\" state=\"ready\" report-to=\"agent/ops/mission-reporter\" stalled-after=\"15m\" { concurrent-runs; goal \"Watched.\"; step \"work\" { agentless } }",
        "publish-watched",
    );
    let source = format!(
        "version 2\nmission-run \"watched/1\" {{\n  mission \"mission/{}@{}\"\n  workspace {:?}\n  requester \"agent/ops/owner\"\n}}\n",
        watched.id,
        watched.revision,
        workspace.path().display().to_string(),
    );
    let declare = || {
        let intent = crate::graph::parse_intent(&source, "node").unwrap();
        let preview = store
            .mission(
                &intent,
                IntentInput {
                    kdl: source.clone(),
                    source_name: None,
                },
            )
            .unwrap();
        (intent, preview)
    };
    let (intent, preview) = declare();
    store
        .apply_as(
            &intent,
            &preview.subject_tokens,
            "start-watched-1",
            Some("agent/ops/owner"),
        )
        .unwrap();
    let run = "mission-run/watched/1";
    let created = store
        .latest_claim(run, Some("mission-run.created"))
        .unwrap()
        .unwrap();
    let reports = || store.claims_for(run, Some(RUN_REPORT_KIND)).unwrap().len();
    let code = |result: Result<MissionRunReportView, St3Error>| result.unwrap_err().code;

    // Only a person or the agent that requested the run may change it, and only to an
    // agent; a limit or completion means nothing without one.
    assert_eq!(
        code(store.set_mission_run_report(
            run,
            "agent/ops/stranger",
            Some("agent/ops/watcher"),
            None,
            None,
            false,
            "stranger",
        )),
        "run-report-authority-denied"
    );
    assert_eq!(
        code(store.set_mission_run_report(
            run,
            "person/operator",
            Some("person/operator"),
            None,
            None,
            false,
            "person-reporter",
        )),
        "invalid-report-to"
    );
    assert_eq!(
        code(store.set_mission_run_report(
            run,
            "person/operator",
            None,
            Some(60_000),
            None,
            false,
            "limit-alone",
        )),
        "report-without-recipient"
    );
    assert_eq!(reports(), 0);

    // The requester names a watcher; the mission's own limit applies unless one is named.
    let changed = store
        .set_mission_run_report(
            run,
            "agent/ops/owner",
            Some("agent/ops/watcher"),
            None,
            None,
            false,
            "owner-sets",
        )
        .unwrap();
    assert!(changed.changed);
    assert_eq!(changed.report_to.as_deref(), Some("agent/ops/watcher"));
    assert_eq!(changed.stalled_after_ms, Some(15 * 60_000));
    assert!(!changed.report_completed);
    // Asking for what it already is writes nothing.
    let again = store
        .set_mission_run_report(
            run,
            "person/operator",
            Some("agent/ops/watcher"),
            None,
            None,
            false,
            "person-repeats",
        )
        .unwrap();
    assert!(!again.changed);
    assert_eq!(reports(), 1);
    let limited = store
        .set_mission_run_report(
            run,
            "person/operator",
            Some("agent/ops/watcher"),
            Some(60 * 60_000),
            None,
            true,
            "person-limits",
        )
        .unwrap();
    assert_eq!(limited.stalled_after_ms, Some(60 * 60_000));
    assert!(limited.report_completed);
    let latest = store
        .latest_claim(run, Some(RUN_REPORT_KIND))
        .unwrap()
        .unwrap();
    assert_eq!(latest.actor.as_deref(), Some("person/operator"));
    assert_eq!(
        run_report_fields(&latest.body),
        Some(("agent/ops/watcher".into(), 60 * 60_000, true, true))
    );

    // Clearing reports to nobody, even though the mission names a reporter.
    let cleared = store
        .set_mission_run_report(run, "agent/ops/owner", None, None, None, false, "owner-clears")
        .unwrap();
    assert!(cleared.changed);
    assert_eq!(cleared.report_to, None);
    let latest = store
        .latest_claim(run, Some(RUN_REPORT_KIND))
        .unwrap()
        .unwrap();
    assert_eq!(run_report_fields(&latest.body), None);
    assert_eq!(reports(), 3);

    // The creation claim is what the run started with, so declaring the run again exactly
    // as it was started is still a retry.
    let still = store
        .latest_claim(run, Some("mission-run.created"))
        .unwrap()
        .unwrap();
    assert_eq!((still.id, still.body), (created.id, created.body));
    let (_, redeclared) = declare();
    assert!(redeclared.blockers.is_empty(), "{:?}", redeclared.blockers);
}

#[test]
fn report_stalls_policy_survives_replication_replay_and_legacy_records() {
    let store = Store::open_memory("source").unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let mission = publish_mission(
        &store,
        r#"version 2
mission "parent" state="ready" report-to="agent/ops/watcher" report-stalls=#false {
  concurrent-runs
  goal "Wait until retirement."
  step "retire" { agentless; gate "retired" { field "state" "mission/parent" is "retired" } }
}"#,
        "muted-mission",
    );
    let run = store
        .create_mission_run(&MissionRunRequest {
            mission: mission.id.clone(),
            revision: Some(mission.revision.clone()),
            workspace: workspace.path().to_string_lossy().into_owned(),
            requester: Some("person/operator".into()),
            mode: None,
            inputs: BTreeMap::new(),
            idempotency_key: "muted-parent".into(),
        })
        .unwrap();
    let created = store
        .latest_claim(&run.subject, Some("mission-run.created"))
        .unwrap()
        .unwrap();
    assert_eq!(created.body["fields"]["report_stalls"], false);
    assert_eq!(
        run_report_fields(&created.body),
        Some(("agent/ops/watcher".into(), 1_800_000, false, false))
    );
    // Omission inherits the pinned mission revision; it is not an implicit re-enable.
    let inherited = store
        .set_mission_run_report(
            &run.subject,
            "person/operator",
            Some("agent/ops/watcher"),
            None,
            None,
            false,
            "inherits-muted",
        )
        .unwrap();
    assert!(!inherited.changed);
    assert!(!inherited.report_stalls);
    for policy in [Some(false), Some(true)] {
        assert_eq!(
            store
                .set_mission_run_report(
                    &run.subject,
                    "person/operator",
                    None,
                    None,
                    policy,
                    false,
                    &format!("no-recipient-{policy:?}")
                )
                .unwrap_err()
                .code,
            "report-without-recipient"
        );
    }
    let replica = Store::open_memory("replica").unwrap();
    replica
        .import_replication("source", &store.export_replication(0).unwrap())
        .unwrap();
    replica.rebuild_claim_projections().unwrap();
    assert!(
        !replica
            .mission_spec("parent", Some(&mission.revision))
            .unwrap()
            .unwrap()
            .report_stalls
    );
    let replayed = replica
        .latest_claim(&run.subject, Some("mission-run.created"))
        .unwrap()
        .unwrap();
    assert_eq!(
        run_report_fields(&replayed.body),
        run_report_fields(&created.body)
    );
    let enabled = store
        .set_mission_run_report(
            &run.subject,
            "person/operator",
            Some("agent/ops/watcher"),
            None,
            Some(true),
            false,
            "explicit-enable",
        )
        .unwrap();
    assert!(enabled.report_stalls);
    replica
        .import_replication("source", &store.export_replication(0).unwrap())
        .unwrap();
    replica.rebuild_claim_projections().unwrap();
    let latest = replica
        .latest_claim(&run.subject, Some(RUN_REPORT_KIND))
        .unwrap()
        .unwrap();
    assert!(latest.body["fields"].get("report_stalls").is_none());
    assert_eq!(
        run_report_fields(&latest.body),
        Some(("agent/ops/watcher".into(), 1_800_000, false, true))
    );
    // Old client responses, requests and creation/change claims still mean stalls=true.
    let old_view = serde_json::json!({"run": run.subject, "report_to": "agent/ops/watcher",
        "stalled_after_ms": 1_800_000, "changed": false});
    let view: MissionRunReportView = serde_json::from_value(old_view).unwrap();
    assert!(view.report_stalls);
    let old_request = serde_json::json!({"actor": "person/operator",
        "report_to": "agent/ops/watcher", "idempotency_key": "old-client"});
    let request: crate::model::MissionRunReportRequest =
        serde_json::from_value(old_request.clone()).unwrap();
    assert_eq!(request.report_stalls, None);
    let mut invalid = old_request;
    invalid["report_stalls"] = json!("false");
    assert!(serde_json::from_value::<crate::model::MissionRunReportRequest>(invalid).is_err());
    let old_claim = json!({"fields": {"report_to": "agent/ops/watcher"}});
    assert_eq!(
        run_report_fields(&old_claim),
        Some(("agent/ops/watcher".into(), 1_800_000, false, true))
    );
}

#[test]
fn a_run_declared_after_another_keeps_its_wait_through_preview_and_replication() {
    let controller = Store::open_memory("controller").unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let build = publish_mission(
        &controller,
        "version 2\nmission \"build\" state=\"ready\" { goal \"Build.\"; step \"build\" { agentless } }",
        "publish-build",
    );
    let ship = publish_mission(
        &controller,
        "version 2\nmission \"ship\" state=\"ready\" { goal \"Ship.\"; step \"ship\" { agentless } }",
        "publish-ship",
    );
    let declare = |run: &str, mission: &MissionSpec, after: Option<&str>| {
        let after = after.map_or(String::new(), |after| format!("  after {after:?}\n"));
        let source = format!(
            "version 2\nmission-run {run:?} {{\n  mission \"mission/{}@{}\"\n  workspace {:?}\n  requester \"person/operator\"\n{after}}}\n",
            mission.id,
            mission.revision,
            workspace.path().display().to_string(),
        );
        let intent = crate::graph::parse_intent(&source, "node").unwrap();
        let preview = controller
            .mission(
                &intent,
                IntentInput {
                    kdl: source,
                    source_name: None,
                },
            )
            .unwrap();
        (intent, preview)
    };

    let (_, early) = declare("ship/1", &ship, Some("build/1"));
    assert_eq!(
        early.blockers,
        [
            "mission run `mission-run/ship/1` waits for `mission-run/build/1`, which does not exist"
        ]
    );
    for (run, mission, after) in [
        ("build/1", &build, None),
        ("ship/1", &ship, Some("build/1")),
    ] {
        let (intent, preview) = declare(run, mission, after);
        assert!(preview.blockers.is_empty(), "{:?}", preview.blockers);
        controller
            .apply_as(
                &intent,
                &preview.subject_tokens,
                &format!("start-{run}"),
                Some("person/operator"),
            )
            .unwrap();
    }
    let (_, changed) = declare("ship/1", &ship, Some("build/2"));
    assert_eq!(
        changed.blockers,
        ["mission run `mission-run/ship/1` already exists with different creation fields"]
    );

    let local = controller.mission_run("ship/1").unwrap().unwrap();
    assert_eq!(local.after.as_deref(), Some("mission-run/build/1"));
    let worker = Store::open_memory("worker").unwrap();
    receive_and_project(
        &worker,
        "controller",
        &exchange_from(&controller, &ReplicationInventory::default()),
    );
    let remote = worker.mission_run("ship/1").unwrap().unwrap();
    assert_eq!(remote.after, local.after);
    assert_eq!(
        remote
            .steps
            .iter()
            .map(|step| (step.subject.as_str(), step.agentless))
            .collect::<Vec<_>>(),
        local
            .steps
            .iter()
            .map(|step| (step.subject.as_str(), step.agentless))
            .collect::<Vec<_>>()
    );
    assert_eq!(remote.steps[0].step, "after-run");
}

#[test]
fn replication_carries_run_generation_lineage() {
    let source = Store::open_memory("source").unwrap();
    let publish = |goal: &str, key: &str| {
        let kdl = format!(
            r#"
version 2

  mission "lineage" state="ready" {{
    goal "Replicate generation lineage."
    agent "owner" {{ workspace "."; command "true" }}
    step "work" {{
      title "Work ${{ST_STEP}} in ${{ST_RUN_GENERATION}}"
      goal {goal:?}
    }}
    step "stable" {{ goal "Carry stable work." }}
    step "waiting" {{ goal "Carry pending work." }}
  }}

"#
        );
        let intent = parse_intent(&kdl, "source").unwrap();
        let planned = source
            .mission(
                &intent,
                IntentInput {
                    kdl,
                    source_name: None,
                },
            )
            .unwrap();
        source.apply(&intent, &planned.subject_tokens, key).unwrap();
        intent.missions["lineage"].clone()
    };
    let first = publish("Use the first goal for ${ST_STEP_RUN}.", "lineage-one");
    let run = source
        .create_mission_run(&MissionRunRequest {
            mission: first.id,
            revision: None,
            workspace: "/tmp".into(),
            requester: Some("person/test".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "lineage-run".into(),
        })
        .unwrap();
    let owner = format!("agent/{}/owner", run.id);
    let stable = run.steps.iter().find(|step| step.step == "stable").unwrap();
    source
        .set_step_state(&stable.subject, "completed", None)
        .unwrap();
    let second = publish("Use the second goal for ${ST_STEP_RUN}.", "lineage-two");
    let revised = source
        .adopt_mission_revision(
            &run.id,
            &second,
            &owner,
            "replicate a successor generation",
            "lineage-cutover",
        )
        .unwrap();

    let target = Store::open_memory("target").unwrap();
    target
        .import_replication("source", &source.export_replication(0).unwrap())
        .unwrap();
    let replicated = target.mission_run(&run.id).unwrap().unwrap();
    assert_eq!(replicated.generation, revised.generation);
    assert_eq!(replicated.revision, second.revision);
    assert_eq!(target.run_generations(&run.id).unwrap().len(), 2);
    assert_eq!(
        replicated
            .steps
            .iter()
            .find(|step| step.step == "work")
            .unwrap()
            .title,
        revised
            .steps
            .iter()
            .find(|step| step.step == "work")
            .unwrap()
            .title
    );
    assert_eq!(
        replicated
            .steps
            .iter()
            .find(|step| step.step == "work")
            .unwrap()
            .goals,
        revised
            .steps
            .iter()
            .find(|step| step.step == "work")
            .unwrap()
            .goals
    );
    assert_eq!(
        replicated
            .steps
            .iter()
            .find(|step| step.step == "stable")
            .unwrap()
            .status,
        "completed"
    );
    assert_eq!(
        revised
            .steps
            .iter()
            .find(|step| step.step == "waiting")
            .unwrap()
            .status,
        "pending"
    );
    assert_eq!(
        replicated
            .steps
            .iter()
            .find(|step| step.step == "waiting")
            .unwrap()
            .status,
        "pending"
    );
    {
        let mut connection = target.connection.lock().unwrap();
        connection
            .execute(
                "UPDATE step_runs SET status='cancelled', blocked_reason='the owning run generation was superseded'
                     WHERE subject=?1",
                [format!("step-run/{}/waiting", revised.generation)],
            )
            .unwrap();
        let transaction = connection.transaction().unwrap();
        project_replicated_mission_runs(&transaction).unwrap();
        transaction.commit().unwrap();
    }
    assert_eq!(
        target
            .mission_run(&run.id)
            .unwrap()
            .unwrap()
            .steps
            .iter()
            .find(|step| step.step == "waiting")
            .unwrap()
            .status,
        "pending",
        "a replay repairs an old carried step projection"
    );
    assert_eq!(
        source
            .replication_status(true, Some(TEST_FLEET), &[])
            .unwrap()
            .graph_digest,
        target
            .replication_status(true, Some(TEST_FLEET), &[])
            .unwrap()
            .graph_digest
    );
}

#[test]
fn replication_carries_revision_proposal_lifecycle() {
    let source = Store::open_memory("source").unwrap();
    let publish = |goal: &str, key: &str| {
        let kdl = format!(
            r#"
version 2

  mission "proposal" state="ready" revisions="human-only" revision-reviewer="person/reviewer" {{
    goal "Replicate a revision proposal."
    agent "owner" {{ workspace "."; command "true" }}
    step "work" {{ goal {goal:?} }}
  }}

"#
        );
        let intent = parse_intent(&kdl, "source").unwrap();
        let planned = source
            .mission(
                &intent,
                IntentInput {
                    kdl,
                    source_name: None,
                },
            )
            .unwrap();
        source.apply(&intent, &planned.subject_tokens, key).unwrap();
        intent.missions["proposal"].clone()
    };
    let first = publish("Use the first goal.", "proposal-one");
    let run = source
        .create_mission_run(&MissionRunRequest {
            mission: first.id,
            revision: None,
            workspace: "/tmp".into(),
            requester: Some("person/requester".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "proposal-run".into(),
        })
        .unwrap();
    let owner = format!("agent/{}/owner", run.id);
    let second = publish("Use the second goal.", "proposal-two");
    let proposal = source
        .create_revision_proposal(
            &run.id,
            &second,
            &owner,
            "replicate the proposal lifecycle",
            "proposal-create",
        )
        .unwrap();
    let target = Store::open_memory("target").unwrap();
    target
        .import_replication("source", &source.export_replication(0).unwrap())
        .unwrap();
    assert_eq!(
        target
            .revision_proposal(&proposal.id)
            .unwrap()
            .unwrap()
            .status,
        "pending-approval"
    );

    source
        .approve_revision_proposal(
            &proposal.id,
            "person/reviewer",
            proposal.preview_hash.as_deref().unwrap(),
            "proposal-approve",
        )
        .unwrap();
    target
        .import_replication(
            "source",
            &source
                .export_replication_for_heads(&target.replica_heads().unwrap())
                .unwrap(),
        )
        .unwrap();
    let replicated = target.revision_proposal(&proposal.id).unwrap().unwrap();
    assert_eq!(replicated.status, "applied");
    assert!(replicated.approvals.contains(&"person/reviewer".into()));
    assert_eq!(
        replicated.successor_generation,
        Some(target.mission_run(&run.id).unwrap().unwrap().generation)
    );
}

#[test]
fn replay_keeps_an_applied_proposal_terminal_beside_a_pending_proposal() {
    let source = Store::open_memory("source").unwrap();
    let publish = |goal: &str, key: &str| {
        let kdl = format!(
            r#"
version 2

mission "proposal-replay" state="ready" revisions="human-only" revision-reviewer="person/reviewer" {{
  goal "Keep proposal states monotonic."
  agent "owner" {{ workspace "."; command "true" }}
  step "work" {{ goal {goal:?} }}
}}
"#
        );
        publish_mission(&source, &kdl, key)
    };
    let first = publish("Use the first goal.", "proposal-replay-one");
    let run = source
        .create_mission_run(&MissionRunRequest {
            mission: first.id,
            revision: None,
            workspace: "/tmp".into(),
            requester: Some("person/requester".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "proposal-replay-run".into(),
        })
        .unwrap();
    let owner = format!("agent/{}/owner", run.id);
    let second = publish("Use the second goal.", "proposal-replay-two");
    let applied = source
        .create_revision_proposal(
            &run.id,
            &second,
            &owner,
            "apply the first revision",
            "proposal-replay-create-a",
        )
        .unwrap();
    source
        .approve_revision_proposal(
            &applied.id,
            "person/reviewer",
            applied.preview_hash.as_deref().unwrap(),
            "proposal-replay-approve-a",
        )
        .unwrap();
    assert_eq!(
        source
            .revision_proposal(&applied.id)
            .unwrap()
            .unwrap()
            .status,
        "applied"
    );

    let third = publish("Use the third goal.", "proposal-replay-three");
    let pending = source
        .create_revision_proposal(
            &run.id,
            &third,
            &owner,
            "leave the next revision pending",
            "proposal-replay-create-b",
        )
        .unwrap();

    let target = Store::open_memory("target").unwrap();
    let exchange = exchange_from(&source, &ReplicationInventory::default());
    receive_and_project(&target, "source", &exchange);
    for _ in 0..3 {
        assert!(target.project_replication_backlog().unwrap());
    }
    assert_eq!(
        target
            .revision_proposal(&applied.id)
            .unwrap()
            .unwrap()
            .status,
        "applied"
    );
    assert_eq!(
        target
            .revision_proposal(&pending.id)
            .unwrap()
            .unwrap()
            .status,
        "pending-approval"
    );
}

#[test]
fn one_invalid_or_unknown_claim_does_not_block_its_valid_sibling_or_later_envelopes() {
    for unknown_kind in [false, true] {
        let source = Store::open_memory("source").unwrap();
        let valid = source
            .append_claim(&ClaimInput {
                subject: "host/source".into(),
                kind: "transport.observed".into(),
                actor: None,
                fields: BTreeMap::from([("status".into(), Value::String("up".into()))]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("valid-sibling".into()),
            })
            .unwrap();
        let mut exchange = exchange_from(&source, &ReplicationInventory::default());
        let candidate = rewrite_envelope(&exchange.envelopes[0], |payload| {
            let mut invalid = payload.batch.claims[0].clone();
            if unknown_kind {
                // A newer transport kind could decide the host's reachability.
                invalid.kind = "transport.future-observed".into();
            } else {
                invalid.body["fields"]["status"] = Value::Bool(true);
            }
            invalid.id = claim_hash(
                &invalid.batch_id,
                &invalid.subject,
                &invalid.kind,
                &invalid.origin,
                invalid.actor.as_deref(),
                &invalid.body,
                &invalid.predecessors,
            )
            .unwrap();
            payload.batch.claims.push(invalid);
        });
        exchange.envelopes = vec![candidate.clone()];
        exchange.inventory.envelopes = vec![ReplicaEnvelopeId {
            writer: candidate.writer.clone(),
            sequence: candidate.sequence,
            hash: candidate.hash.clone(),
        }];

        let target = Store::open_memory("target").unwrap();
        let admission = receive_and_project(&target, "source", &exchange);
        assert_eq!(admission.valid, 1);
        assert_eq!(admission.invalid, usize::from(!unknown_kind));
        assert_eq!(admission.unknown, usize::from(unknown_kind));
        assert_eq!(
            target
                .latest_claim("host/source", Some("transport.observed"))
                .unwrap()
                .unwrap()
                .id,
            valid.id
        );
        assert_eq!(
            target.status(Some("host/source")).unwrap().subjects[0].reachability,
            "indeterminate"
        );

        source
            .append_claim(&ClaimInput {
                subject: "host/later".into(),
                kind: "transport.observed".into(),
                actor: None,
                fields: BTreeMap::from([("status".into(), Value::String("up".into()))]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("later-envelope".into()),
            })
            .unwrap();
        let later = exchange_from(&source, &target.replication_inventory().unwrap());
        receive_and_project(&target, "source", &later);
        assert!(
            target
                .latest_claim("host/later", Some("transport.observed"))
                .unwrap()
                .is_some()
        );
        assert_eq!(target.replica_records(true).unwrap().len(), 1);
    }
}

#[test]
fn review_reader_old_peer_quarantines_and_retries_original_bytes() {
    for kind in std::iter::once("gate.requested").chain(st3_schema::review::KINDS) {
        let current = Store::open_memory("ada").unwrap();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("review.sqlite");
        let older = Store::open(&path, "tester").unwrap();
        let mut registry = st3_schema::registry().clone();
        for new_kind in st3_schema::review::KINDS {
            registry.claims.remove(new_kind);
        }
        for name in st3_schema::review::REQUEST_FIELDS {
            registry
                .claims
                .get_mut("gate.requested")
                .unwrap()
                .fields
                .remove(name);
        }
        assert_ne!(registry.digest(), st3_schema::registry().digest());
        older.set_claim_registry(registry);
        let observe = |store: &Store, subject: &str, status: &str| {
            store
                .append_claim(&ClaimInput {
                    subject: subject.into(),
                    kind: "transport.observed".into(),
                    actor: None,
                    fields: BTreeMap::from([("status".into(), json!(status))]),
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: None,
                })
                .unwrap()
        };
        let bootstrap = observe(&current, "host/ada", "up");
        receive_and_project(
            &older,
            "ada",
            &exchange_from(&current, &ReplicationInventory::default()),
        );
        current.project_replication_backlog().unwrap();
        assert!(!current.replication_projection_needs_recovery().unwrap());
        // An empty store's initial replay is legitimate. Everything after this
        // healthy baseline, including reopening with the new registry, must be incremental.
        assert!(!older.replication_projection_needs_recovery().unwrap());
        FULL_REPLAYS.with(|count| count.set(0));
        const DAY_MS: u128 = 24 * 60 * 60 * 1_000;
        current
            .set_write_clock_at(bootstrap.accepted_at_unix_ms + DAY_MS)
            .unwrap();
        let spec = st3_schema::registry().claim(kind).unwrap();
        let mut fields = BTreeMap::new();
        for (name, field) in &spec.fields {
            if !field.required { continue; }
            let value = match field.value_type {
                st3_schema::ValueType::Integer => json!(1),
                st3_schema::ValueType::Array => json!([]),
                st3_schema::ValueType::Object => json!({}),
                st3_schema::ValueType::SubjectReference => json!(format!("{}/tester", field.reference_families.first().map_or("mission-run", String::as_str))),
                _ => json!(field.values.first().map_or("receipt", String::as_str)),
            };
            fields.insert(name.clone(), value);
        }
        if matches!(kind, "gate.head-bound" | "gate.review-verdict" | "gate.requested") {
            fields.insert("change".into(), json!({"kind":"github.pull-request","subject":"resource/project/review"}));
        }
        if matches!(kind, "gate.head-bound" | "gate.requested") {
            fields.insert("head".into(), json!({"kind":"git.commit","sha":"a".repeat(40)}));
        }
        if matches!(kind, "gate.review-verdict" | "gate.remediation-attached") {
            fields.insert("reviewed_head".into(), json!({"kind":"git.commit","sha":"a".repeat(40)}));
        }
        if kind == "gate.requested" {
            fields.extend(BTreeMap::from([
                ("gate_subject".into(),json!("gate-operation/tester")),
                ("generation".into(),json!("run-generation/tester")), ("episode".into(),json!(1)),
                ("scope".into(),json!({"coverage":["implementation"]})),
                ("author".into(),json!("agent/alex")), ("binding_claim".into(),json!("binding")),
                ("delta_ref".into(),json!("doc/project/delta")),
            ]));
        }
        let claim = current.append_claim(&ClaimInput {
            subject:"gate-operation/tester".into(), kind:kind.into(), actor:None,
            fields, evidence:Vec::new(), expected_subject:None, idempotency_key:None,
        }).unwrap();
        let exchange = exchange_from(&current, &older.replication_inventory().unwrap());
        assert_eq!(exchange.envelopes.len(), 1);
        let original_envelope = &exchange.envelopes[0];
        // Hold the old review envelope while the old peer makes ninety days of
        // unrelated progress. No sleeps or wall-clock timing can weaken this lag.
        for day in 2..=90 {
            older
                .set_write_clock_at(bootstrap.accepted_at_unix_ms + day * DAY_MS)
                .unwrap();
            observe(&older, "host/tester", "up");
        }
        older.project_replication_backlog().unwrap();
        let admission = receive_and_project(&older, "ada", &exchange);
        assert_eq!((admission.valid, admission.unknown, admission.invalid), (0,1,0), "{kind}");
        let records = older.replica_records(true).unwrap();
        assert_eq!(records.len(),1);
        assert_eq!(records[0].state,"unknown");
        assert_eq!(records[0].error_code.as_deref(),Some(if kind == "gate.requested" { "unknown-claim-field" } else { "unknown-claim-kind" }));
        let raw: Vec<u8> = older.readers.get().query_row(
            "SELECT raw FROM replica_records WHERE claim_id=?1",[&claim.id],|row|row.get(0),
        ).unwrap();
        assert!(!raw.is_empty());
        assert!(older.claims_for(&claim.subject,Some(kind)).unwrap().is_empty());
        assert_eq!(FULL_REPLAYS.with(std::cell::Cell::get), 0, "{kind}: lagged receipt");
        assert!(!older.replication_projection_needs_recovery().unwrap());
        // Known remote work after the unknown record must still cross the same
        // writer chain and advance the projection over several inventory pages.
        for day in 2..=25 {
            current
                .set_write_clock_at(bootstrap.accepted_at_unix_ms + day * DAY_MS)
                .unwrap();
            observe(&current, "host/ada", if day == 25 { "down" } else { "up" });
        }
        let mut pages = 0;
        loop {
            let mut inventory = older.replication_inventory().unwrap();
            inventory.accepts = Some(4);
            let page = exchange_from(&current, &inventory);
            if page.envelopes.is_empty() {
                break;
            }
            assert!(pages < 6, "{kind}: catch-up inventory must advance");
            pages += 1;
            let admission = receive_and_project(&older, "ada", &page);
            assert_eq!(admission.invalid, 0, "{kind}");
            assert!(admission.valid > 0, "{kind}");
            assert_eq!(FULL_REPLAYS.with(std::cell::Cell::get), 0, "{kind}: catch-up page");
            assert!(!older.replication_projection_needs_recovery().unwrap());
        }
        assert_eq!(pages, 6, "{kind}: exercise paginated catch-up");
        let latest = older.latest_claim("host/ada", Some("transport.observed")).unwrap().unwrap();
        assert_eq!(latest.body["fields"]["status"], "down", "{kind}: unrelated work progressed");
        assert_eq!(older.replica_records(true).unwrap().len(), 1, "{kind}");
        assert!(older.claims_for(&claim.subject, Some(kind)).unwrap().is_empty());
        drop(older);
        let upgraded = Store::open(&path,"tester").unwrap();
        assert!(!upgraded.replication_projection_needs_recovery().unwrap());
        let admitted = upgraded.validate_replication_backlog().unwrap();
        assert_eq!((admitted.valid,admitted.unknown,admitted.invalid),(1,0,0));
        upgraded.project_replication_backlog().unwrap();
        let retained = upgraded.claims_for(&claim.subject,Some(kind)).unwrap();
        assert_eq!(retained.len(),1);
        assert_eq!(retained[0].id,claim.id);
        assert_eq!(retained[0].body,claim.body);
        let retried_raw: Vec<u8> = upgraded.readers.get().query_row(
            "SELECT raw FROM replica_records WHERE claim_id=?1",[&claim.id],|row|row.get(0),
        ).unwrap();
        assert_eq!(raw,retried_raw);
        assert!(upgraded.replica_records(true).unwrap().is_empty(), "{kind}");
        assert_eq!(
            upgraded.latest_claim("host/ada", Some("transport.observed")).unwrap().unwrap().id,
            latest.id,
            "{kind}: retry preserves unrelated projection"
        );
        let relayed = exchange_from(&upgraded, &ReplicationInventory::default());
        let retained_envelope = relayed.envelopes.iter().find(|envelope| {
            envelope.writer == original_envelope.writer
                && envelope.sequence == original_envelope.sequence
        }).unwrap();
        assert_eq!(retained_envelope.hash, original_envelope.hash, "{kind}");
        assert_eq!(
            retained_envelope.payload.bytes().unwrap(),
            original_envelope.payload.bytes().unwrap(),
            "{kind}: retry and relay retain the original hash-authenticated envelope bytes"
        );
        assert!(!upgraded.replication_projection_needs_recovery().unwrap());
        assert_eq!(FULL_REPLAYS.with(std::cell::Cell::get), 0, "{kind}: upgrade retry");
    }
}

#[test]
fn an_unknown_field_is_retryable_and_an_old_schema_rejection_is_readmitted() {
    let source = Store::open_memory("source").unwrap();
    let claim = source
        .append_claim(&ClaimInput {
            subject: "host/source".into(),
            kind: "transport.observed".into(),
            actor: None,
            fields: BTreeMap::from([("status".into(), Value::String("up".into()))]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some("schema-upgrade-source".into()),
        })
        .unwrap();
    let exchange = exchange_from(&source, &ReplicationInventory::default());
    let envelope = &exchange.envelopes[0];

    let target = Store::open_memory("target").unwrap();
    target.bind_fleet(TEST_FLEET).unwrap();
    target
        .receive_replication_exchange("source", TEST_FLEET, &exchange)
        .unwrap();
    let record_ref = replica_record_ref(&envelope.writer, envelope.sequence, &envelope.hash, 0);
    {
        let connection = target.connection.lock().unwrap();
        connection
            .execute(
                "UPDATE replica_envelopes SET receipt_state='degraded'\n                     WHERE writer=?1 AND sequence=?2 AND envelope_hash=?3",
                params![envelope.writer, envelope.sequence, envelope.hash],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO replica_records(\n                         record_ref, writer, sequence, envelope_hash, position, raw, state,\n                         claim_id, subject_hint, kind_hint, error_code, error_message, updated_at_unix_ms\n                     ) VALUES (?1, ?2, ?3, ?4, 0, X'', 'invalid', ?5, ?6, ?7,\n                               'invalid-replicated-claim', ?8, '1')",
                params![
                    record_ref,
                    envelope.writer,
                    envelope.sequence,
                    envelope.hash,
                    claim.id,
                    claim.subject,
                    claim.kind,
                    format!(
                        "replicated claim `{}` violates unknown-claim-field: field `future` is unknown",
                        claim.id
                    ),
                ],
            )
            .unwrap();
    }

    let admission = target.validate_replication_backlog().unwrap();
    assert_eq!(admission.valid, 1);
    assert_eq!(admission.unknown, 0);
    assert_eq!(admission.invalid, 0);
    assert!(target.replica_records(true).unwrap().is_empty());
    assert_eq!(
        target
            .latest_claim("host/source", Some("transport.observed"))
            .unwrap()
            .unwrap()
            .id,
        claim.id
    );
}

#[test]
fn a_claim_field_from_a_newer_schema_is_unknown_instead_of_invalid() {
    let source = Store::open_memory("source").unwrap();
    source
        .append_claim(&ClaimInput {
            subject: "host/source".into(),
            kind: "transport.observed".into(),
            actor: None,
            fields: BTreeMap::from([("status".into(), Value::String("up".into()))]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some("future-field-source".into()),
        })
        .unwrap();
    let mut exchange = exchange_from(&source, &ReplicationInventory::default());
    let candidate = rewrite_envelope(&exchange.envelopes[0], |payload| {
        let claim = &mut payload.batch.claims[0];
        claim.body["fields"]["future"] = Value::Bool(true);
        claim.id = claim_hash(
            &claim.batch_id,
            &claim.subject,
            &claim.kind,
            &claim.origin,
            claim.actor.as_deref(),
            &claim.body,
            &claim.predecessors,
        )
        .unwrap();
    });
    exchange.envelopes = vec![candidate.clone()];
    exchange.inventory.envelopes = vec![ReplicaEnvelopeId {
        writer: candidate.writer.clone(),
        sequence: candidate.sequence,
        hash: candidate.hash.clone(),
    }];

    let target = Store::open_memory("target").unwrap();
    let admission = receive_and_project(&target, "source", &exchange);
    assert_eq!(admission.valid, 0);
    assert_eq!(admission.unknown, 1);
    assert_eq!(admission.invalid, 0);
    let unresolved = target.replica_records(true).unwrap();
    assert_eq!(unresolved.len(), 1);
    assert_eq!(unresolved[0].state, "unknown");
    assert_eq!(
        unresolved[0].error_code.as_deref(),
        Some("unknown-claim-field")
    );
}

#[test]
fn future_arrangement_vocabulary_waits_on_replication_but_is_refused_locally() {
    let subject = "arrangement/person/ada/019a0000-0000-7000-8000-000000000001";
    for (operation, code) in [
        (json!({"op":"future.rename","name":"new"}), "invalid-arrangement-operations"),
        (json!({"op":"subject.place","subject":"future/item","folder":null,"key":"a0"}), "invalid-subject-reference"),
    ] {
        let source = Store::open_memory("source").unwrap();
        let mut input = ClaimInput {
            subject: subject.into(), kind: "arrangement.edited".into(),
            actor: Some("person/ada".into()),
            fields: BTreeMap::from([
                ("owner".into(), json!("person/ada")),
                ("operations".into(), json!([{"op":"create","name":"original"}])),
            ]),
            evidence: Vec::new(), expected_subject: None, idempotency_key: None,
        };
        source.append_claim(&input).unwrap();
        input.fields.insert("operations".into(), json!([operation.clone()]));
        assert_eq!(source.append_claim(&input).unwrap_err().code, code);
        let mut exchange = exchange_from(&source, &ReplicationInventory::default());
        let candidate = rewrite_envelope(&exchange.envelopes[0], |payload| {
            let claim = &mut payload.batch.claims[0];
            claim.body["fields"]["operations"] = json!([operation]);
            claim.id = claim_hash(&claim.batch_id, &claim.subject, &claim.kind,
                &claim.origin, claim.actor.as_deref(), &claim.body, &claim.predecessors).unwrap();
        });
        exchange.envelopes = vec![candidate.clone()];
        exchange.inventory.envelopes = vec![ReplicaEnvelopeId {
            writer: candidate.writer.clone(), sequence: candidate.sequence, hash: candidate.hash.clone(),
        }];
        let target = Store::open_memory("target").unwrap();
        let admission = receive_and_project(&target, "source", &exchange);
        assert_eq!((admission.valid, admission.unknown, admission.invalid), (0, 1, 0));
        let records = target.replica_records(true).unwrap();
        assert_eq!(records[0].state, "unknown");
        assert_eq!(records[0].error_code.as_deref(), Some("unknown-claim-field"));
        assert!(target.latest_claim(subject, None).unwrap().is_none());
    }
}

#[test]
fn arrangement_vocabulary_upgrade_applies_the_original_waiting_claim() {
    let subject = "arrangement/person/ada/019a0000-0000-7000-8000-000000000001";
    for operation in [
        json!({"op":"rename","name":"upgraded"}),
        json!({"op":"subject.place","subject":"mission/future","folder":null,"key":"a0"}),
    ] {
        let source = Store::open_memory("source").unwrap();
        let mut input = ClaimInput {
            subject: subject.into(), kind: "arrangement.edited".into(), actor: Some("person/ada".into()),
            fields: BTreeMap::from([("owner".into(), json!("person/ada")),
                ("operations".into(), json!([{"op":"create","name":"original"}]))]),
            evidence: Vec::new(), expected_subject: None, idempotency_key: None,
        };
        source.append_claim(&input).unwrap();
        input.fields.insert("operations".into(), json!([operation.clone()]));
        let claim = source.append_claim(&input).unwrap();
        let exchange = exchange_from(&source, &ReplicationInventory::default());
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("claims.sqlite3");
        let older = Store::open(&path, "target").unwrap();
        let mut registry = st3_schema::registry().clone();
        if operation["op"] == "rename" {
            registry.arrangement_operations.retain(|tag| tag != "rename");
        } else {
            registry.subjects.remove("mission").unwrap();
        }
        older.set_claim_registry(registry);
        let admission = receive_and_project(&older, "source", &exchange);
        assert_eq!((admission.valid, admission.unknown, admission.invalid), (1, 1, 0));
        let records = older.replica_records(true).unwrap();
        assert_eq!(records[0].state, "unknown");
        assert_eq!(records[0].claim_id.as_deref(), Some(claim.id.as_str()));
        assert_eq!(records[0].error_code.as_deref(), Some("unknown-claim-field"));
        drop(older);

        let upgraded = Store::open(&path, "target").unwrap();
        let admission = upgraded.validate_replication_backlog().unwrap();
        assert_eq!(admission.invalid, 0);
        assert!(upgraded.replica_records(true).unwrap().is_empty());
        upgraded.rebuild_claim_projections().unwrap();
        let resource = upgraded.arrangement(subject, upgraded.index().unwrap()).unwrap().unwrap();
        if operation["op"] == "rename" {
            assert_eq!(resource["body"]["name"]["value"], "upgraded");
            assert_eq!(resource["body"]["name"]["revision"], claim.id);
        } else {
            assert_eq!(resource["body"]["placements"]["mission/future"]["value"]["key"], "a0");
            assert_eq!(resource["body"]["placements"]["mission/future"]["revision"], claim.id);
        }
        assert_eq!(upgraded.latest_claim(subject, None).unwrap().unwrap().id, claim.id);
    }
}

#[test]
fn one_invalid_blob_does_not_block_an_unrelated_valid_claim() {
    let source = Store::open_memory("source").unwrap();
    let valid = source
        .append_claim(&ClaimInput {
            subject: "host/source".into(),
            kind: "transport.observed".into(),
            actor: None,
            fields: BTreeMap::from([("status".into(), Value::String("up".into()))]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some("valid-beside-invalid-blob".into()),
        })
        .unwrap();
    let exchange = exchange_from(&source, &ReplicationInventory::default());
    let candidate = rewrite_envelope(&exchange.envelopes[0], |payload| {
        payload
            .blobs
            .insert("0".repeat(64), b"wrong bytes".to_vec());
    });
    let input = ReplicationExchange {
        build: None,
        projection_digests: Default::default(),
        peer: "source".into(),
        fleet_id: TEST_FLEET.into(),
        schema_digest: runtime::compatibility_digest(&st3_schema::registry().digest()),
        projection_registry_digest: None,
        projection_versions: Default::default(),
        authority_digest: String::new(),
        graph_digest: String::new(),
        inventory: ReplicationInventory {
            digest: String::new(),
            envelopes: vec![ReplicaEnvelopeId {
                writer: candidate.writer.clone(),
                sequence: candidate.sequence,
                hash: candidate.hash.clone(),
            }],
            buckets: Vec::new(),
            accepts: None,
            checkpoint: None,
        },
        envelopes: vec![candidate],
        signature_requests: Vec::new(),
        signatures: Vec::new(),
    };

    let target = Store::open_memory("target").unwrap();
    let admission = receive_and_project(&target, "source", &input);
    assert_eq!(admission.valid, 1);
    assert_eq!(admission.invalid, 1);
    assert_eq!(
        target
            .latest_claim("host/source", Some("transport.observed"))
            .unwrap()
            .unwrap()
            .id,
        valid.id
    );
    let records = target.replica_records(true).unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].kind.as_deref(), Some("blob"));
    assert_eq!(records[0].error_code.as_deref(), Some("blob-hash-mismatch"));
}

#[test]
fn same_sequence_candidates_converge_in_any_receipt_order() {
    let source = Store::open_memory("source").unwrap();
    source
        .append_claim(&ClaimInput {
            subject: "host/source".into(),
            kind: "transport.observed".into(),
            actor: None,
            fields: BTreeMap::from([("status".into(), Value::String("up".into()))]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some("candidate-source".into()),
        })
        .unwrap();
    let original =
        exchange_from(&source, &ReplicationInventory::default()).envelopes[0].clone();
    let fork = rewrite_envelope(&original, |payload| {
        let claim = &mut payload.batch.claims[0];
        claim.kind = "future.transport-observed".into();
        claim.id = claim_hash(
            &claim.batch_id,
            &claim.subject,
            &claim.kind,
            &claim.origin,
            claim.actor.as_deref(),
            &claim.body,
            &claim.predecessors,
        )
        .unwrap();
    });
    let make_exchange = |envelopes: Vec<ReplicaEnvelope>| ReplicationExchange {
        build: None,
        projection_digests: Default::default(),
        peer: "source".into(),
        fleet_id: TEST_FLEET.into(),
        schema_digest: runtime::compatibility_digest(&st3_schema::registry().digest()),
        projection_registry_digest: None,
        projection_versions: Default::default(),
        authority_digest: String::new(),
        graph_digest: String::new(),
        inventory: ReplicationInventory {
            digest: String::new(),
            envelopes: envelopes
                .iter()
                .map(|envelope| ReplicaEnvelopeId {
                    writer: envelope.writer.clone(),
                    sequence: envelope.sequence,
                    hash: envelope.hash.clone(),
                })
                .collect(),
            buckets: Vec::new(),
            accepts: None,
            checkpoint: None,
        },
        envelopes,
        signature_requests: Vec::new(),
        signatures: Vec::new(),
    };
    let left = Store::open_memory("left").unwrap();
    let right = Store::open_memory("right").unwrap();
    receive_and_project(
        &left,
        "source",
        &make_exchange(vec![original.clone(), fork.clone()]),
    );
    receive_and_project(&right, "source", &make_exchange(vec![fork, original]));
    let left_status = left
        .replication_status(true, Some(TEST_FLEET), &[])
        .unwrap();
    let right_status = right
        .replication_status(true, Some(TEST_FLEET), &[])
        .unwrap();
    assert_eq!(left_status.received_envelopes, 2);
    assert_eq!(left_status.authority_digest, right_status.authority_digest);
    assert_eq!(left_status.graph_digest, right_status.graph_digest);
    assert_eq!(left_status.unknown_records, 1);
    assert_eq!(right_status.unknown_records, 1);
}

/// One exchange carrying exactly `envelopes`, as a peer that holds only those would send it.
pub(super) fn exchange_of(peer: &str, envelopes: Vec<ReplicaEnvelope>) -> ReplicationExchange {
    ReplicationExchange {
        build: None,
        projection_digests: Default::default(),
        peer: peer.into(),
        fleet_id: TEST_FLEET.into(),
        schema_digest: runtime::compatibility_digest(&st3_schema::registry().digest()),
        projection_registry_digest: None,
        projection_versions: Default::default(),
        authority_digest: String::new(),
        graph_digest: String::new(),
        inventory: ReplicationInventory {
            digest: String::new(),
            envelopes: envelopes
                .iter()
                .map(|envelope| ReplicaEnvelopeId {
                    writer: envelope.writer.clone(),
                    sequence: envelope.sequence,
                    hash: envelope.hash.clone(),
                })
                .collect(),
            buckets: Vec::new(),
            accepts: None,
            checkpoint: None,
        },
        envelopes,
        signature_requests: Vec::new(),
        signatures: Vec::new(),
    }
}

#[test]
fn planning_state_and_attention_agree_when_claims_arrive_in_different_orders() {
    let source = Store::open_memory("source").unwrap();
    let kdl = "version 2\nmission \"release\" state=\"ready\" { goal \"Ship a release\" }\n";
    let intent = crate::graph::parse_test_intent(kdl, "source").unwrap();
    let mission = source
        .mission(
            &intent,
            IntentInput {
                kdl: kdl.into(),
                source_name: None,
            },
        )
        .unwrap();
    let doc = source
        .put_document(
            "doc/planning/release/request",
            b"Ship a release",
            &None,
            "request",
        )
        .unwrap();
    let reference = format!("{}@{}", doc.name, doc.hash);
    let subject = "planning-session/planning/release/one";
    let events = [
        (
            "started",
            json!({"mission": "mission/release", "request": reference, "workspace": "/work/release", "requester": "person/avery", "planner": "agent/source.planner"}),
        ),
        (
            "candidate-submitted",
            json!({"candidate_revision": 1, "markdown": reference, "kdl": reference, "mission_revision": "revision-one"}),
        ),
        (
            "previewed",
            json!({"candidate_revision": 1, "preview_hash": "preview-one", "store_index": mission.store_index, "graph": "release", "diff": "new", "mission": mission}),
        ),
        (
            "approved",
            json!({"mission_revision": "revision-one", "requester": "person/avery"}),
        ),
    ];
    for (kind, fields) in events {
        // Distinct accepted times also prove this is independent of the receiver's indexes.
        std::thread::sleep(std::time::Duration::from_millis(2));
        source
            .append_claim(&ClaimInput {
                subject: subject.into(),
                kind: format!("planning-session.{kind}"),
                actor: Some("person/avery".into()),
                fields: serde_json::from_value(fields).unwrap(),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some(format!("planning-{kind}")),
            })
            .unwrap();
        if kind == "previewed" {
            source.replay_replication_graph().unwrap();
            let attention = source.attention_items(Some("person/avery")).unwrap();
            assert_eq!(attention.len(), 1);
            assert_eq!(attention[0].kind, "launch-approval");
        }
    }
    let envelopes = exchange_from(&source, &ReplicationInventory::default()).envelopes;
    let in_order = Store::open_memory("in-order").unwrap();
    receive_and_project(
        &in_order,
        "source",
        &exchange_of("source", envelopes.clone()),
    );
    let reversed = Store::open_memory("reversed").unwrap();
    for envelope in envelopes.iter().rev() {
        receive_and_project(
            &reversed,
            "source",
            &exchange_of("source", vec![envelope.clone()]),
        );
    }
    assert_eq!(graph_digest_of(&in_order), graph_digest_of(&reversed));
    for store in [&in_order, &reversed] {
        let launch = store.planning_session(subject).unwrap().unwrap();
        assert_eq!(launch.status, "approved");
        assert_eq!(launch.published_revision.as_deref(), Some("revision-one"));
        assert!(
            store
                .attention_items(Some("person/avery"))
                .unwrap()
                .is_empty()
        );
        // Startup replay must retain the same result, too.
        store.replay_replication_graph().unwrap();
        assert_eq!(
            store.planning_session(subject).unwrap().unwrap().status,
            "approved"
        );
        assert!(
            store
                .attention_items(Some("person/avery"))
                .unwrap()
                .is_empty()
        );
    }
}

/// Status, the harness view and the latest claim of a kind fold in canonical order. Two nodes
/// that hold the same claims therefore agree even when the claims reached them in different
/// orders, which a checkpoint relies on when it drops claims that a later one replaced.
#[test]
fn nodes_that_received_the_same_claims_in_different_orders_agree() {
    let source = Store::open_memory("source").unwrap();
    let agent = "agent/source.worker";
    for (status, incarnation) in [
        ("running", "inc-0"),
        ("exited", "inc-0"),
        ("running", "inc-1"),
    ] {
        source
            .append_claim(&ClaimInput {
                subject: agent.into(),
                kind: "runtime.observed".into(),
                actor: None,
                fields: BTreeMap::from([
                    ("status".into(), Value::String(status.into())),
                    ("incarnation_id".into(), Value::String(incarnation.into())),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some(format!("runtime:{status}:{incarnation}")),
            })
            .unwrap();
    }
    for (state, observed_at) in [("idle", 1_000), ("working", 2_000), ("idle", 3_000)] {
        source
            .append_claim_outcome(&harness_state(agent, state, observed_at))
            .unwrap();
    }
    let envelopes = exchange_from(&source, &ReplicationInventory::default()).envelopes;
    assert!(envelopes.len() >= 6);

    let in_order = Store::open_memory("in-order").unwrap();
    receive_and_project(
        &in_order,
        "source",
        &exchange_of("source", envelopes.clone()),
    );
    // The other node admits every envelope in its own exchange, newest first, so its store
    // indexes run backwards.
    let reversed = Store::open_memory("reversed").unwrap();
    for envelope in envelopes.iter().rev() {
        receive_and_project(
            &reversed,
            "source",
            &exchange_of("source", vec![envelope.clone()]),
        );
    }
    let first = |store: &Store| {
        store
            .claims_for(agent, Some("runtime.observed"))
            .unwrap()
            .first()
            .map(|claim| claim.store_index)
    };
    assert!(
        first(&reversed) > first(&in_order),
        "the reversed node must have admitted the oldest claim last"
    );

    for store in [&in_order, &reversed] {
        let actual = store.latest_actual_value(agent).unwrap().unwrap();
        assert_eq!(actual["status"], "running");
        assert_eq!(actual["incarnation_id"], "inc-1");
        let harness = store.current_harness(agent).unwrap().unwrap();
        assert_eq!(harness.state, "idle");
    }
    assert_eq!(
        in_order
            .latest_claim(agent, Some("runtime.observed"))
            .unwrap()
            .unwrap()
            .id,
        reversed
            .latest_claim(agent, Some("runtime.observed"))
            .unwrap()
            .unwrap()
            .id,
    );
    let ids = |store: &Store| {
        store
            .claims_for(agent, None)
            .unwrap()
            .into_iter()
            .map(|claim| claim.id)
            .collect::<Vec<_>>()
    };
    assert_eq!(ids(&in_order), ids(&reversed));
    assert_eq!(
        in_order
            .latest_claim(agent, Some("harness.observed"))
            .unwrap()
            .unwrap()
            .body["fields"]["state"],
        "idle"
    );
    assert_eq!(
        reversed
            .latest_claim(agent, Some("harness.observed"))
            .unwrap()
            .unwrap()
            .body["fields"]["state"],
        "idle"
    );
}

fn graph_digest_of(store: &Store) -> String {
    store
        .replication_status(true, Some(TEST_FLEET), &[])
        .unwrap()
        .graph_digest
}

fn synced_takeover_pair() -> (Store, Store, MissionRunView) {
    let source = Store::open_memory("source").unwrap();
    let replica = Store::open_memory("replica").unwrap();
    let failed = failed_takeover_run(&source, &["deploy-check"]);
    receive_and_project(
        &replica,
        "source",
        &exchange_from(&source, &replica.replication_inventory().unwrap()),
    );
    assert_eq!(graph_digest_of(&replica), graph_digest_of(&source));
    (source, replica, failed)
}

/// Put one step of every run on another attempt, as the carried-step lookup by batch did.
fn misproject_a_step(store: &Store) {
    let connection = store.connection.lock().unwrap();
    connection
        .execute(
            "UPDATE step_runs SET attempt=attempt+1
                 WHERE subject=(SELECT subject FROM step_runs ORDER BY subject LIMIT 1)",
            [],
        )
        .unwrap();
}

#[test]
fn a_first_sync_ends_once_both_hold_the_same_claims() {
    let source = Store::open_memory("source").unwrap();
    failed_takeover_run(&source, &["deploy-check"]);
    let newcomer = Store::open_memory("newcomer").unwrap();
    newcomer.begin_first_sync("source").unwrap();
    receive_and_project(
        &newcomer,
        "source",
        &exchange_from(&source, &newcomer.replication_inventory().unwrap()),
    );
    assert_eq!(
        newcomer.first_sync().unwrap().unwrap().state,
        "syncing",
        "an exchange that stores envelopes has not admitted them yet"
    );

    let summary = source.export_replication_summary(TEST_FLEET).unwrap();
    newcomer
        .receive_replication_exchange_asking("source", TEST_FLEET, &summary, true)
        .unwrap();

    let first = newcomer.first_sync().unwrap().unwrap();
    assert_eq!(first.state, "verified");
    assert_eq!(first.peer.as_deref(), Some("source"));
    assert_eq!(
        first.envelopes,
        Some(source.replication_inventory().unwrap().envelopes.len() as u64)
    );
    assert_eq!(first.message, None);
}

/// Projections are this node's own pure functions of its claims. A first sync that ends
/// with a graph unlike the peer's settles on the claims, records no defect, and neither
/// replays nor repairs either graph.
#[test]
fn a_first_sync_with_a_different_graph_settles_on_claims_without_repair() {
    let (source, newcomer, _) = synced_takeover_pair();
    newcomer.begin_first_sync("source").unwrap();
    misproject_a_step(&newcomer);
    let (newcomer_graph, source_graph) = (graph_digest_of(&newcomer), graph_digest_of(&source));
    assert_ne!(newcomer_graph, source_graph);
    let summary = source.export_replication_summary(TEST_FLEET).unwrap();
    for asks in [false, true] {
        newcomer
            .receive_replication_exchange_asking("source", TEST_FLEET, &summary, asks)
            .unwrap();
    }
    let status = newcomer
        .replication_status(true, Some(TEST_FLEET), &["source".to_owned()])
        .unwrap();
    let first = status.first_sync.unwrap();
    assert_eq!((first.state.as_str(), first.message), ("verified", None));
    assert_eq!(
        (graph_digest_of(&newcomer), graph_digest_of(&source)),
        (newcomer_graph, source_graph),
        "neither graph is replayed or repaired"
    );
}

#[test]
fn the_same_envelopes_projecting_different_graphs_diverge() {
    let left = Store::open_memory("left").unwrap();
    let right = Store::open_memory("right").unwrap();
    let intent = simple("true");
    let preview = left
        .mission(
            &intent,
            IntentInput {
                kdl: "work".into(),
                source_name: None,
            },
        )
        .unwrap();
    left.apply(&intent, &preview.subject_tokens, "work")
        .unwrap();
    receive_and_project(
        &right,
        "left",
        &exchange_from(&left, &right.replication_inventory().unwrap()),
    );
    receive_and_project(
        &left,
        "right",
        &exchange_from(&right, &left.replication_inventory().unwrap()),
    );
    // Each exchange a worker starts carries its node's inventory and graph digest.
    let summary = |from: &Store, to: &Store, relay: &str| {
        let exchange = from.export_replication_summary(TEST_FLEET).unwrap();
        to.receive_replication_exchange(relay, TEST_FLEET, &exchange)
            .unwrap();
        to.replication_peer_sync(&[relay.to_owned()])
            .remove(relay)
            .unwrap()
    };
    let synced = summary(&left, &right, "left");
    assert_eq!(
        (synced.peer_only_envelopes, synced.local_only_envelopes),
        (0, 0)
    );
    assert!(synced.graph_compared_at_unix_ms.is_some());
    assert_eq!(synced.graph_differs_since_unix_ms, None);
    assert!(!synced.diverged);

    // Right loses the desired claim while keeping its envelope, as a node that drops
    // received claims would: the inventories still match, the graphs no longer do.
    {
        let connection = right.connection.lock().unwrap();
        connection
            .execute_batch(
                "CREATE TEMP TABLE dropped_claims AS
                         SELECT * FROM claims WHERE subject='exec/work' AND kind='intent.desired';
                     CREATE TEMP TABLE dropped_desired AS
                         SELECT * FROM desired WHERE subject='exec/work';
                     PRAGMA foreign_keys=OFF;
                     DELETE FROM desired WHERE subject='exec/work';
                     DELETE FROM claims WHERE subject='exec/work' AND kind='intent.desired';
                     PRAGMA foreign_keys=ON;",
            )
            .unwrap();
    }
    assert_eq!(
        left.replication_inventory().unwrap().digest,
        right.replication_inventory().unwrap().digest
    );
    let differs = summary(&left, &right, "left");
    let since = differs.graph_differs_since_unix_ms.expect("graphs differ");
    assert_eq!(differs.graph_compared_at_unix_ms, Some(since));
    assert!(
        !differs.diverged,
        "one comparison can catch a peer mid-projection"
    );
    assert!(!right.replication_diverged());
    assert!(
        summary(&right, &left, "right")
            .graph_differs_since_unix_ms
            .is_some(),
        "the peer sees the difference too"
    );

    // A difference that outlasts any projection delay is divergence.
    right
        .replication_sync
        .lock()
        .unwrap()
        .get_mut("left")
        .unwrap()
        .graph_differs_since_unix_ms = Some(since - REPLICATION_DIVERGED_AFTER_MS);
    let diverged = summary(&left, &right, "left");
    assert!(diverged.diverged);
    assert_eq!(
        diverged.graph_differs_since_unix_ms,
        Some(since - REPLICATION_DIVERGED_AFTER_MS)
    );
    assert!(right.replication_diverged());
    let status = right
        .replication_status(true, Some(TEST_FLEET), &["left".to_owned()])
        .unwrap();
    assert!(status.peers[0].sync.as_ref().unwrap().diverged);
    assert_ne!(
        status.peers[0].graph_digest.as_deref(),
        Some(status.graph_digest.as_str())
    );

    // Different envelopes cannot be compared, so they leave the last comparison standing.
    let compared = diverged.graph_compared_at_unix_ms;
    left.record_transport_observation("right", "up", None, None)
        .unwrap();
    let moved = summary(&left, &right, "left");
    assert_eq!(moved.peer_only_envelopes, 1);
    assert_eq!(moved.graph_compared_at_unix_ms, compared);
    assert!(moved.diverged);

    // Once the same envelopes project the same graph again, the divergence clears.
    receive_and_project(
        &right,
        "left",
        &exchange_from(&left, &right.replication_inventory().unwrap()),
    );
    {
        let connection = right.connection.lock().unwrap();
        connection
            .execute_batch(
                "PRAGMA foreign_keys=OFF;
                     INSERT INTO claims SELECT * FROM dropped_claims;
                     INSERT INTO desired SELECT * FROM dropped_desired;
                     PRAGMA foreign_keys=ON;",
            )
            .unwrap();
    }
    let healed = summary(&left, &right, "left");
    assert_eq!(
        (healed.peer_only_envelopes, healed.local_only_envelopes),
        (0, 0)
    );
    assert_eq!(healed.graph_differs_since_unix_ms, None);
    assert!(!healed.diverged);
    assert!(!right.replication_diverged());
}

#[test]
fn concurrent_graph_writes_converge_after_a_partition() {
    let left = Store::open_memory("left").unwrap();
    let right = Store::open_memory("right").unwrap();
    let initial = simple("true");
    let initial_preview = left
        .mission(
            &initial,
            IntentInput {
                kdl: "initial".into(),
                source_name: None,
            },
        )
        .unwrap();
    left.apply(&initial, &initial_preview.subject_tokens, "initial")
        .unwrap();
    let initial_exchange = exchange_from(&left, &ReplicationInventory::default());
    receive_and_project(&right, "left", &initial_exchange);

    let left_change = simple("printf-left");
    let left_preview = left
        .mission(
            &left_change,
            IntentInput {
                kdl: "left".into(),
                source_name: None,
            },
        )
        .unwrap();
    left.apply(&left_change, &left_preview.subject_tokens, "left-change")
        .unwrap();

    let right_change = simple("printf-right");
    let right_preview = right
        .mission(
            &right_change,
            IntentInput {
                kdl: "right".into(),
                source_name: None,
            },
        )
        .unwrap();
    right
        .apply(&right_change, &right_preview.subject_tokens, "right-change")
        .unwrap();

    let to_right = exchange_from(&left, &right.replication_inventory().unwrap());
    receive_and_project(&right, "left", &to_right);
    let to_left = exchange_from(&right, &left.replication_inventory().unwrap());
    receive_and_project(&left, "right", &to_left);

    let left_status = left
        .replication_status(true, Some(TEST_FLEET), &[])
        .unwrap();
    let right_status = right
        .replication_status(true, Some(TEST_FLEET), &[])
        .unwrap();
    assert_eq!(left_status.authority_digest, right_status.authority_digest);
    assert_eq!(left_status.graph_digest, right_status.graph_digest);
    assert_eq!(
        left.selected_desired_revision("exec/work").unwrap(),
        right.selected_desired_revision("exec/work").unwrap()
    );
}

#[test]
fn an_explicit_repair_keeps_the_bad_record_and_names_its_replacement() {
    let source = Store::open_memory("source").unwrap();
    let replacement = source
        .append_claim(&ClaimInput {
            subject: "host/source".into(),
            kind: "transport.observed".into(),
            actor: None,
            fields: BTreeMap::from([("status".into(), Value::String("up".into()))]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some("repair-replacement".into()),
        })
        .unwrap();
    let mut exchange = exchange_from(&source, &ReplicationInventory::default());
    let original = exchange.envelopes[0].clone();
    let broken = rewrite_envelope(&original, |payload| {
        let claim = &mut payload.batch.claims[0];
        claim.body["fields"]["unexpected"] = Value::Bool(true);
        claim.id = claim_hash(
            &claim.batch_id,
            &claim.subject,
            &claim.kind,
            &claim.origin,
            claim.actor.as_deref(),
            &claim.body,
            &claim.predecessors,
        )
        .unwrap();
    });
    exchange.envelopes = vec![original, broken];
    let target = Store::open_memory("target").unwrap();
    receive_and_project(&target, "source", &exchange);
    let record = target.replica_records(true).unwrap().remove(0);
    let first = target
        .repair_replica_record(
            &record.record_ref,
            &replacement.id,
            "replace the malformed observation",
            "person/operator",
            "repair-once",
        )
        .unwrap();
    let retry = target
        .repair_replica_record(
            &record.record_ref,
            &replacement.id,
            "replace the malformed observation",
            "person/operator",
            "repair-once",
        )
        .unwrap();
    assert_eq!(first.id, retry.id);
    let repaired = target.replica_record(&record.record_ref).unwrap().unwrap();
    assert_eq!(repaired.state, "repaired");
    assert_eq!(repaired.replacement_claim_id, Some(replacement.id));
    assert!(target.replica_records(true).unwrap().is_empty());

    let repair_exchange = exchange_from(&target, &source.replication_inventory().unwrap());
    receive_and_project(&source, "target", &repair_exchange);
    let replicated = source.replica_record(&record.record_ref).unwrap().unwrap();
    assert_eq!(replicated.state, "repaired");
    assert!(source.replica_records(true).unwrap().is_empty());
}

#[test]
fn a_replicated_repair_replaces_a_record_that_the_source_had_accepted() {
    let source = Store::open_memory("source").unwrap();
    let target = Store::open_memory("target").unwrap();

    let initial = simple("true");
    let preview = source
        .mission(
            &initial,
            IntentInput {
                kdl: "initial".into(),
                source_name: None,
            },
        )
        .unwrap();
    source
        .apply(&initial, &preview.subject_tokens, "initial")
        .unwrap();
    receive_and_project(
        &target,
        "source",
        &exchange_from(&source, &target.replication_inventory().unwrap()),
    );
    let replacement = source.selected_desired_token("exec/work").unwrap().unwrap();

    let changed = simple("printf changed");
    let preview = source
        .mission(
            &changed,
            IntentInput {
                kdl: "changed".into(),
                source_name: None,
            },
        )
        .unwrap();
    source
        .apply(&changed, &preview.subject_tokens, "changed")
        .unwrap();
    receive_and_project(
        &target,
        "source",
        &exchange_from(&source, &target.replication_inventory().unwrap()),
    );
    let repaired_claim = source.selected_desired_token("exec/work").unwrap().unwrap();
    assert_ne!(repaired_claim, replacement);

    let record_ref = {
        let connection = target.connection.lock().unwrap();
        let record_ref = connection
            .query_row(
                "SELECT record_ref FROM replica_records WHERE claim_id=?1",
                [&repaired_claim],
                |row| row.get::<_, String>(0),
            )
            .unwrap();
        connection
            .execute(
                "UPDATE replica_records SET state='invalid' WHERE record_ref=?1",
                [&record_ref],
            )
            .unwrap();
        record_ref
    };
    target
        .repair_replica_record(
            &record_ref,
            &replacement,
            "the receiver rejects this record after an upgrade",
            "person/operator",
            "repair-version-skew",
        )
        .unwrap();
    assert_eq!(
        target.selected_desired_token("exec/work").unwrap().unwrap(),
        replacement
    );
    // A build that rejected the original never put it in claims. Keep the raw repaired
    // record, but model that admission difference explicitly.
    target
        .connection
        .lock()
        .unwrap()
        .execute("DELETE FROM claims WHERE id=?1", [&repaired_claim])
        .unwrap();

    receive_and_project(
        &source,
        "target",
        &exchange_from(&target, &source.replication_inventory().unwrap()),
    );
    assert_eq!(
        source.selected_desired_token("exec/work").unwrap().unwrap(),
        replacement
    );
    let repaired = source
        .replica_records(false)
        .unwrap()
        .into_iter()
        .find(|record| record.claim_id.as_deref() == Some(repaired_claim.as_str()))
        .unwrap();
    assert_eq!(repaired.state, "repaired");
    assert_eq!(
        repaired.replacement_claim_id.as_deref(),
        Some(replacement.as_str())
    );

    let source_status = source
        .replication_status(true, Some(TEST_FLEET), &[])
        .unwrap();
    let target_status = target
        .replication_status(true, Some(TEST_FLEET), &[])
        .unwrap();
    assert_eq!(
        source_status.authority_digest,
        target_status.authority_digest
    );
    assert_eq!(source_status.graph_digest, target_status.graph_digest);
}

#[test]
fn repaired_original_sources_and_operations_ignore_local_retention_and_roll_back() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("claims.sqlite3");
    let source = Store::open(&path, "alder").unwrap();
    let target = Store::open_memory("birch").unwrap();
    let observation = |reason: &str, key: &str| {
        source
            .append_claim(&ClaimInput {
                subject: "daemon/alder".into(),
                kind: "daemon.diagnostic".into(),
                actor: None,
                fields: BTreeMap::from([
                    ("severity".into(), json!("warning")),
                    ("code".into(), json!("repair-test")),
                    ("reason".into(), json!(reason)),
                ]),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: Some(key.into()),
            })
            .unwrap()
    };
    let replacement = observation("replacement", "replacement");
    let original = observation("original", "original");
    receive_and_project(
        &target,
        "alder",
        &exchange_from(&source, &ReplicationInventory::default()),
    );
    let record = target
        .replica_records(false)
        .unwrap()
        .into_iter()
        .find(|record| record.claim_id.as_deref() == Some(&original.id))
        .unwrap();
    let before = projection_digest::tables(&source.readers.get()).unwrap();
    {
        let mut connection = source.connection.lock().unwrap();
        let transaction = connection.transaction().unwrap();
        transaction
            .execute(
                "UPDATE replica_records SET state='repaired' WHERE claim_id=?1",
                [&original.id],
            )
            .unwrap();
        rebuild_operations_tx(&transaction).unwrap();
        let current = projection_digest::tables(&transaction).unwrap();
        assert_ne!(current["claim_sources"], before["claim_sources"]);
        assert_ne!(current["operations"], before["operations"]);
        assert_eq!(current, projection_digest::oracle(&transaction).unwrap());
        transaction.rollback().unwrap();
    }
    assert_eq!(
        projection_digest::tables(&source.readers.get()).unwrap(),
        before
    );
    target
        .connection
        .lock()
        .unwrap()
        .execute(
            "UPDATE replica_records SET state='unknown' WHERE record_ref=?1",
            [&record.record_ref],
        )
        .unwrap();
    target
        .repair_replica_record(
            &record.record_ref,
            &replacement.id,
            "replace an unsupported original",
            "person/robin",
            "repair",
        )
        .unwrap();
    target
        .connection
        .lock()
        .unwrap()
        .execute("DELETE FROM claims WHERE id=?1", [&original.id])
        .unwrap();
    receive_and_project(
        &source,
        "birch",
        &exchange_from(&target, &source.replication_inventory().unwrap()),
    );
    let expected = projection_digest::tables(&target.readers.get()).unwrap();
    assert_eq!(
        projection_digest::tables(&source.readers.get()).unwrap(),
        expected
    );
    assert_eq!(
        expected,
        projection_digest::oracle(&target.readers.get()).unwrap()
    );
    assert!(source.claim_by_id(&original.id).unwrap().is_some());
    assert!(target.claim_by_id(&original.id).unwrap().is_none());
    assert_eq!(
        source.latest_actual_value("daemon/alder").unwrap(),
        target.latest_actual_value("daemon/alder").unwrap(),
        "retaining a repaired original must not change actual state"
    );
    for store in [&source, &target] {
        let index = store.index().unwrap();
        let connection = store.readers.get();
        assert_eq!(
            selected_actual_source_at(&connection, "daemon/alder", Some(index), None)
                .unwrap().0,
            Some(replacement.id.clone()),
            "actual provenance must select the replacement's surviving claim"
        );
    }
    let expected_actual = target.latest_actual_value("daemon/alder").unwrap();
    // Receipt cleanup must not make a repaired original a source again.
    source
        .connection
        .lock()
        .unwrap()
        .execute(
            "DELETE FROM replica_records WHERE claim_id=?1",
            [&original.id],
        )
        .unwrap();
    assert_eq!(
        projection_digest::tables(&source.readers.get()).unwrap(),
        expected
    );
    assert_eq!(
        projection_digest::oracle(&source.readers.get()).unwrap(),
        expected
    );
    assert_eq!(source.latest_actual_value("daemon/alder").unwrap(), expected_actual);
    // Registry rebuilds and restart retain the repair meaning after receipts were trimmed.
    source
        .connection
        .lock()
        .unwrap()
        .execute(
            "UPDATE meta SET value='old-registry' WHERE key='projection_digest_registry'",
            [],
        )
        .unwrap();
    drop(source);
    let source = Store::open(&path, "alder").unwrap();
    assert_eq!(source.latest_actual_value("daemon/alder").unwrap(), expected_actual);
    assert_eq!(
        projection_digest::tables(&source.readers.get()).unwrap(),
        expected
    );
    assert_eq!(
        projection_digest::oracle(&source.readers.get()).unwrap(),
        expected
    );
}

proptest! {
    #[test]
    fn envelope_sets_converge_after_sparse_duplicate_delivery(
        reverse in any::<bool>(),
        copies in 1usize..4,
        split_seed in 0usize..8,
    ) {
        let source = Store::open_memory("source").unwrap();
        for index in 0..4 {
            source
                .append_claim(&ClaimInput {
                    subject: format!("host/source-{index}"),
                    kind: "transport.observed".into(),
                    actor: None,
                    fields: BTreeMap::from([("status".into(), Value::String("up".into()))]),
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: Some(format!("source-{index}")),
                })
                .unwrap();
        }
        let complete = exchange_from(&source, &ReplicationInventory::default());
        let mut delivered = complete.envelopes.clone();
        if reverse {
            delivered.reverse();
        }
        let split = split_seed % (delivered.len() + 1);
        let second = delivered.split_off(split);
        let duplicate = |part: &[ReplicaEnvelope]| {
            let mut envelopes = Vec::new();
            for envelope in part {
                envelopes.extend(std::iter::repeat_n(envelope.clone(), copies));
            }
            ReplicationExchange {
                envelopes,
                ..complete.clone()
            }
        };
        let target = Store::open_memory("target").unwrap();
        receive_and_project(&target, "source", &duplicate(&delivered));
        receive_and_project(&target, "source", &duplicate(&second));
        receive_and_project(&target, "source", &complete);
        let source_status = source
            .replication_status(true, Some(TEST_FLEET), &[])
            .unwrap();
        let target_status = target
            .replication_status(true, Some(TEST_FLEET), &[])
            .unwrap();
        prop_assert_eq!(source_status.authority_digest, target_status.authority_digest);
        prop_assert_eq!(source_status.graph_digest, target_status.graph_digest);
        prop_assert_eq!(target_status.pending_records, 0);
        prop_assert_eq!(target_status.invalid_records, 0);
        prop_assert_eq!(target_status.unknown_records, 0);
    }
}

#[test]
fn replication_rejects_tampered_claims() {
    let source = Store::open_memory("source").unwrap();
    source
        .append_claim(&ClaimInput {
            subject: "host/source".into(),
            kind: "transport.observed".into(),
            actor: None,
            fields: BTreeMap::from([("status".into(), Value::String("up".into()))]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    let mut batch = source.export_replication(0).unwrap();
    batch.batches[0].claims[0].body["fields"]["status"] = Value::String("down".into());
    let target = Store::open_memory("target").unwrap();
    let error = target
        .import_replication("source", &batch)
        .expect_err("tampering must fail");
    assert_eq!(error.code, "claim-hash-mismatch");
}

/// One source claim whose body lists its fields in reverse order and whose ID is the hash of
/// that order, the way builds between eaec66a and 2537978d wrote claims on 2026-09-16.
fn insertion_order_hashed_batch() -> ReplicationBatch {
    let source = Store::open_memory("source").unwrap();
    source
        .append_claim(&ClaimInput {
            subject: "host/source".into(),
            kind: "transport.observed".into(),
            actor: None,
            fields: BTreeMap::from([
                ("protocol".into(), Value::String("http-replication".into())),
                ("status".into(), Value::String("up".into())),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    let mut batch = source.export_replication(0).unwrap();
    let claim = &mut batch.batches[0].claims[0];
    let reversed = |value: &Value| {
        let mut fields = value.as_object().unwrap().iter().collect::<Vec<_>>();
        fields.sort_by(|left, right| right.0.cmp(left.0));
        Value::Object(
            fields
                .into_iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
        )
    };
    let mut body = reversed(&claim.body);
    body["fields"] = reversed(&claim.body["fields"]);
    claim.body = body;
    claim.id = canonical_hash(&(
        &claim.batch_id,
        &claim.subject,
        &claim.kind,
        &claim.origin,
        claim.actor.as_deref(),
        &claim.body,
        &claim.predecessors,
    ))
    .unwrap();
    batch
}

#[test]
fn a_claim_hashed_in_field_insertion_order_still_verifies() {
    let batch = insertion_order_hashed_batch();
    let claim = &batch.batches[0].claims[0];
    let canonical = claim_hash(
        &claim.batch_id,
        &claim.subject,
        &claim.kind,
        &claim.origin,
        claim.actor.as_deref(),
        &claim.body,
        &claim.predecessors,
    )
    .unwrap();
    assert_ne!(canonical, claim.id, "the fixture must not be canonical");
    assert!(claim_id_is_content_hash(claim).unwrap());

    // A new node decodes the body from CBOR. It must keep the writer's field order.
    let mut bytes = Vec::new();
    ciborium::into_writer(claim, &mut bytes).unwrap();
    let decoded: ClaimRecord = ciborium::from_reader(bytes.as_slice()).unwrap();
    assert!(claim_id_is_content_hash(&decoded).unwrap());

    let mut tampered = decoded;
    tampered.body["fields"]["status"] = Value::String("down".into());
    assert!(!claim_id_is_content_hash(&tampered).unwrap());

    let target = Store::open_memory("target").unwrap();
    target.import_replication("source", &batch).unwrap();
    assert!(target.claim_by_id(&claim.id).unwrap().is_some());
}

#[test]
fn a_node_that_rejected_insertion_order_hashes_admits_them_once_upgraded() {
    const FLEET: &str = "018f6f0d-4a5d-7b8c-9d0e-123456789abc";
    let envelope = |batch: &ReplicaBatch| {
        let mut bytes = Vec::new();
        ciborium::into_writer(
            &ReplicaEnvelopePayload {
                batch: batch.clone(),
                blobs: BTreeMap::new(),
                claim_signatures: BTreeMap::new(),
            },
            &mut bytes,
        )
        .unwrap();
        ReplicaEnvelope {
            writer: batch.origin.clone(),
            sequence: batch.replica_sequence,
            previous_hash: batch.previous_hash.clone(),
            hash: replica_envelope_hash(
                &batch.origin,
                batch.replica_sequence,
                batch.previous_hash.as_deref(),
                batch.accepted_at_unix_ms,
                &bytes,
            ),
            accepted_at_unix_ms: batch.accepted_at_unix_ms,
            payload: bytes.into(),
            member_key: None,
            signature: None,
        }
    };
    let receive = |store: &Store, envelope: ReplicaEnvelope| {
        store
            .receive_replication_exchange(
                "source",
                FLEET,
                &ReplicationExchange {
                    build: None,
                    projection_digests: Default::default(),
                    peer: "source".into(),
                    fleet_id: FLEET.into(),
                    schema_digest: String::new(),
                    projection_registry_digest: None,
                    projection_versions: Default::default(),
                    authority_digest: String::new(),
                    graph_digest: String::new(),
                    inventory: ReplicationInventory::default(),
                    envelopes: vec![envelope],
                    signature_requests: Vec::new(),
                    signatures: Vec::new(),
                },
            )
            .unwrap();
    };
    let batch = insertion_order_hashed_batch().batches.remove(0);
    let legacy = envelope(&batch);
    let target = Store::open_memory("target").unwrap();
    receive(&target, legacy.clone());
    // An older build rejected the genuine claim as a hash mismatch.
    record_invalid_replica_envelope(
        &target.connection.lock().unwrap(),
        &legacy,
        &St3Error::new("claim-hash-mismatch", "failed verification"),
    )
    .unwrap();
    assert_eq!(target.replica_records(true).unwrap().len(), 1);

    let admission = target.validate_replication_backlog().unwrap();
    assert_eq!(admission.invalid, 0);
    assert!(target.replica_records(true).unwrap().is_empty());
    assert!(target.claim_by_id(&batch.claims[0].id).unwrap().is_some());

    // A claim that fails both hashes stays invalid and is not checked again on every wake.
    let mut tampered = batch.clone();
    tampered.replica_sequence += 1;
    tampered.hash = batch_header_hash(
        &tampered.origin,
        tampered.replica_sequence,
        tampered.previous_hash.as_deref(),
        tampered.accepted_at_unix_ms,
    )
    .unwrap();
    tampered.id = format!(
        "batch/{}/{}/{}",
        tampered.origin, tampered.replica_sequence, tampered.hash
    );
    tampered.claims[0].batch_id = tampered.id.clone();
    tampered.claims[0].body["fields"]["status"] = Value::String("down".into());
    receive(&target, envelope(&tampered));
    assert_eq!(target.validate_replication_backlog().unwrap().invalid, 1);
    assert_eq!(target.validate_replication_backlog().unwrap().invalid, 0);
    assert_eq!(target.replica_records(true).unwrap().len(), 1);
}

#[test]
fn replication_rejects_a_registered_claim_that_violates_the_schema() {
    let source = Store::open_memory("source").unwrap();
    source
        .append_claim(&ClaimInput {
            subject: "host/source".into(),
            kind: "transport.observed".into(),
            actor: None,
            fields: BTreeMap::from([("status".into(), Value::String("up".into()))]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    let mut batch = source.export_replication(0).unwrap();
    let claim = &mut batch.batches[0].claims[0];
    claim.body["fields"]["unexpected"] = Value::Bool(true);
    claim.id = claim_hash(
        &claim.batch_id,
        &claim.subject,
        &claim.kind,
        &claim.origin,
        claim.actor.as_deref(),
        &claim.body,
        &claim.predecessors,
    )
    .unwrap();

    let target = Store::open_memory("target").unwrap();
    let error = target
        .import_replication("source", &batch)
        .expect_err("a registered claim must satisfy the shared schema");
    assert_eq!(error.code, "invalid-replicated-claim");
    assert_eq!(target.index().unwrap(), 0);
}

#[test]
fn an_unknown_replicated_claim_marks_its_subject_indeterminate() {
    let source = Store::open_memory("source").unwrap();
    source
        .append_claim(&ClaimInput {
            subject: "resource/future".into(),
            kind: "resource.observed".into(),
            actor: None,
            fields: BTreeMap::from([
                ("kind".into(), Value::String("custom.st3.future".into())),
                ("status".into(), Value::String("active".into())),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some("future".into()),
        })
        .unwrap();
    let mut batch = source.export_replication(0).unwrap();
    let claim = &mut batch.batches[0].claims[0];
    claim.kind = "future.resource-observed".into();
    claim.id = claim_hash(
        &claim.batch_id,
        &claim.subject,
        &claim.kind,
        &claim.origin,
        claim.actor.as_deref(),
        &claim.body,
        &claim.predecessors,
    )
    .unwrap();

    let target = Store::open_memory("target").unwrap();
    target.import_replication("source", &batch).unwrap();
    let status = target.status(Some("resource/future")).unwrap();
    assert_eq!(status.subjects[0].reachability, "indeterminate");
    assert!(
        status.subjects[0]
            .reason
            .as_deref()
            .unwrap()
            .contains("future.resource-observed")
    );
}

#[test]
fn replication_relays_each_origin_across_multiple_peers() {
    let source = Store::open_memory("source").unwrap();
    source
        .append_claim(&ClaimInput {
            subject: "host/source".into(),
            kind: "transport.observed".into(),
            actor: None,
            fields: BTreeMap::from([("status".into(), Value::String("up".into()))]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some("source-up".into()),
        })
        .unwrap();

    let middle = Store::open_memory("middle").unwrap();
    middle
        .import_replication("source", &source.export_replication(0).unwrap())
        .unwrap();
    let relayed = middle
        .export_replication_for_heads(&BTreeMap::new())
        .unwrap();
    assert_eq!(relayed.peer, "middle");
    assert_eq!(relayed.batches[0].origin, "source");

    let target = Store::open_memory("target").unwrap();
    target.import_replication("middle", &relayed).unwrap();
    assert!(
        target
            .latest_claim("host/source", Some("transport.observed"))
            .unwrap()
            .is_some()
    );
    assert_eq!(target.replica_heads().unwrap().get("source"), Some(&1));
}

#[test]
fn a_later_publish_cites_and_resolves_all_concurrent_intent_heads() {
    let left = Store::open_memory("left").unwrap();
    let initial = simple("true");
    let mission = left
        .mission(
            &initial,
            IntentInput {
                kdl: "initial".into(),
                source_name: None,
            },
        )
        .unwrap();
    left.apply(&initial, &mission.subject_tokens, "initial")
        .unwrap();

    let right = Store::open_memory("right").unwrap();
    right
        .import_replication("left", &left.export_replication(0).unwrap())
        .unwrap();
    let left_change = simple("false");
    let right_change = simple("printf right");
    let left_mission = left
        .mission(
            &left_change,
            IntentInput {
                kdl: "left".into(),
                source_name: None,
            },
        )
        .unwrap();
    let right_mission = right
        .mission(
            &right_change,
            IntentInput {
                kdl: "right".into(),
                source_name: None,
            },
        )
        .unwrap();
    left.apply(&left_change, &left_mission.subject_tokens, "left")
        .unwrap();
    right
        .apply(&right_change, &right_mission.subject_tokens, "right")
        .unwrap();

    let left_heads = left.replica_heads().unwrap();
    let right_heads = right.replica_heads().unwrap();
    left.import_replication(
        "right",
        &right.export_replication_for_heads(&left_heads).unwrap(),
    )
    .unwrap();
    right
        .import_replication(
            "left",
            &left.export_replication_for_heads(&right_heads).unwrap(),
        )
        .unwrap();
    assert_eq!(
        left.status(Some("exec/work")).unwrap().subjects[0]
            .conflicts
            .len(),
        1
    );
    assert_eq!(
        right.status(Some("exec/work")).unwrap().subjects[0]
            .conflicts
            .len(),
        1
    );

    let resolved = simple("printf resolved");
    let resolution_mission = left
        .mission(
            &resolved,
            IntentInput {
                kdl: "resolved".into(),
                source_name: None,
            },
        )
        .unwrap();
    assert_eq!(resolution_mission.subject_tokens["exec/work"].len(), 2);
    left.apply(&resolved, &resolution_mission.subject_tokens, "resolved")
        .unwrap();
    let right_heads = right.replica_heads().unwrap();
    right
        .import_replication(
            "left",
            &left.export_replication_for_heads(&right_heads).unwrap(),
        )
        .unwrap();
    assert!(
        left.status(Some("exec/work")).unwrap().subjects[0]
            .conflicts
            .is_empty()
    );
    assert!(
        right.status(Some("exec/work")).unwrap().subjects[0]
            .conflicts
            .is_empty()
    );
    assert_eq!(
        left.status(Some("exec/work")).unwrap().subjects[0].desired_revision,
        right.status(Some("exec/work")).unwrap().subjects[0].desired_revision
    );
}

#[test]
fn capabilities_are_one_use_and_expire() {
    let store = Store::open_memory("node").unwrap();
    let (secret, _) = store
        .issue_capability("terminal", "agent/node.worker", Some("one"), 1_000)
        .unwrap();
    assert!(!store.consume_capability(&secret, "terminal").unwrap().used);
    assert!(store.consume_capability(&secret, "terminal").unwrap().used);

    let (expired, _) = store
        .issue_capability("terminal", "agent/node.worker", Some("one"), 0)
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(2));
    assert_eq!(
        store
            .consume_capability(&expired, "terminal")
            .expect_err("expired capability")
            .code,
        "expired-capability"
    );
}

/// The message index follows the claim log: each message's first claim, closed once a claim
/// closes it. A message whose claims a checkpoint removes is indexed again from the rest, and
/// a store written before the index existed fills it once from its claims.
#[test]
fn the_message_index_follows_the_message_claims() {
    let store = Store::open_memory("node").unwrap();
    let append = |subject: &str, kind: &str, status: &str, actor: &str| {
        store
            .append_claim(&ClaimInput {
                subject: subject.into(),
                kind: kind.into(),
                actor: Some(actor.into()),
                fields: BTreeMap::from([("status".into(), Value::String(status.into()))]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some(format!("{subject}-{kind}")),
            })
            .unwrap()
    };
    for message in 0..3 {
        append(
            &format!("message/index-{message}"),
            "message.sent",
            "sent",
            "requester",
        );
    }
    for (kind, status) in [
        ("message.delivered", "delivered"),
        ("message.read", "read"),
        ("message.closed", "closed"),
    ] {
        append("message/index-0", kind, status, "agent/worker");
    }
    let indexed = |store: &Store| {
        let connection = store.readers.get();
        let rows = |sql: &str| {
            connection
                .prepare(sql)
                .unwrap()
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, u64>(1)?,
                        row.get::<_, bool>(2)?,
                    ))
                })
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
        };
        (
            rows("SELECT subject, created_index, closed FROM message_index ORDER BY subject"),
            rows(
                "SELECT subject, MIN(store_index), MAX(kind = 'message.closed') FROM claims
                     WHERE subject LIKE 'message/%' GROUP BY subject ORDER BY subject",
            ),
        )
    };
    let (index, claims) = indexed(&store);
    assert_eq!(index, claims);
    assert_eq!(index.iter().filter(|(_, _, closed)| *closed).count(), 1);
    let open = store
        .messages(None, false)
        .unwrap()
        .into_iter()
        .map(|message| message.subject)
        .collect::<Vec<_>>();
    assert_eq!(open, ["message/index-1", "message/index-2"]);
    assert_eq!(store.messages(None, true).unwrap().len(), 3);

    // A checkpoint removes the rows that refer to a claim before the claim itself.
    store
        .connection
        .write()
        .execute_batch(
            "PRAGMA foreign_keys = OFF;
                 DELETE FROM claims WHERE subject='message/index-0' AND kind='message.closed';
                 PRAGMA foreign_keys = ON;",
        )
        .unwrap();
    let (index, claims) = indexed(&store);
    assert_eq!(index, claims);
    assert_eq!(store.messages(None, false).unwrap().len(), 3);

    {
        let connection = store.connection.write();
        connection
            .execute_batch(
                "DELETE FROM message_index; DELETE FROM meta WHERE key='message_index';",
            )
            .unwrap();
        backfill_message_index(&connection).unwrap();
    }
    let (index, claims) = indexed(&store);
    assert_eq!(index, claims);
}

#[test]
fn message_lifecycle_exposes_native_staging_without_breaking_direct_delivery() {
    let store = Store::open_memory("node").unwrap();
    let subject = "message/demo";
    let append = |kind: &str, status: &str, actor: Option<&str>, key: &str| {
        store.append_claim(&ClaimInput {
            subject: subject.into(),
            kind: kind.into(),
            actor: actor.map(str::to_owned),
            fields: BTreeMap::from([("status".into(), Value::String(status.into()))]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some(key.into()),
        })
    };
    append("message.sent", "sent", Some("requester"), "sent").unwrap();
    assert_eq!(
        append("message.closed", "closed", Some("agent/worker"), "early")
            .expect_err("close must not skip states")
            .code,
        "invalid-message-transition"
    );
    append("message.staged", "staged", Some("agent/worker"), "staged").unwrap();
    assert_eq!(store.messages(None, true).unwrap()[0].status, "staged");
    append(
        "message.delivered",
        "delivered",
        Some("agent/worker"),
        "delivered",
    )
    .unwrap();
    let read = append("message.read", "read", Some("agent/worker"), "read").unwrap();
    let repeated_read =
        append("message.read", "read", Some("agent/worker"), "read-again").unwrap();
    assert_eq!(repeated_read.id, read.id);
    let closed = append("message.closed", "closed", Some("agent/worker"), "closed").unwrap();
    let repeated_close = append(
        "message.closed",
        "closed",
        Some("agent/worker"),
        "closed-again",
    )
    .unwrap();
    assert_eq!(repeated_close.id, closed.id);
    assert_eq!(store.messages(None, true).unwrap()[0].status, "closed");
    assert!(store.messages(None, false).unwrap().is_empty());

    let direct = "message/direct";
    let append_direct = |kind: &str, status: &str, key: &str| {
        store.append_claim(&ClaimInput {
            subject: direct.into(),
            kind: kind.into(),
            actor: Some("agent/worker".into()),
            fields: BTreeMap::from([("status".into(), Value::String(status.into()))]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some(key.into()),
        })
    };
    append_direct("message.sent", "sent", "direct-sent").unwrap();
    append_direct("message.delivered", "delivered", "direct-delivered").unwrap();
}

#[test]
fn only_the_person_recipient_can_read_before_native_delivery() {
    let store = Store::open_memory("person-read").unwrap();
    for (id, to) in [
        ("sent", "person/avery"),
        ("staged", "person/avery"),
        ("native", "agent/example/keeper"),
    ] {
        let subject = format!("message/{id}");
        store
            .append_claim(&ClaimInput {
                subject: subject.clone(),
                kind: "message.sent".into(),
                actor: Some("agent/example/sender".into()),
                fields: BTreeMap::from([
                    ("status".into(), Value::String("sent".into())),
                    ("from".into(), Value::String("agent/example/sender".into())),
                    ("to".into(), Value::String(to.into())),
                ]),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        let receipt = |actor: &str, status: &str| {
            store.append_claim(&ClaimInput {
                subject: subject.clone(),
                kind: format!("message.{status}"),
                actor: Some(actor.into()),
                fields: BTreeMap::from([("status".into(), Value::String(status.into()))]),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
        };
        for actor in ["person/other", "agent/example/sender"] {
            assert_eq!(
                receipt(actor, "read").unwrap_err().code,
                "invalid-message-transition"
            );
            assert_eq!(store.message(&subject).unwrap().unwrap().status, "sent");
        }
        if id == "native" {
            assert_eq!(
                receipt(to, "read").unwrap_err().code,
                "invalid-message-transition"
            );
            continue;
        }
        if id == "staged" {
            receipt(to, "staged").unwrap();
        }
        let first = receipt(to, "read").unwrap();
        assert_eq!(receipt(to, "read").unwrap().id, first.id);
        assert_eq!(store.message(&subject).unwrap().unwrap().status, "read");
    }
}

#[test]
fn native_mailbox_pages_use_exact_recipient_and_stable_created_cursors() {
    let store = Store::open_memory("node").unwrap();
    for (id, to) in [
        ("first", "agent/worker"),
        ("other", "agent/other"),
        ("second", "agent/worker"),
    ] {
        store
            .append_claim(&ClaimInput {
                subject: format!("message/{id}"),
                kind: "message.sent".into(),
                actor: Some("person/test".into()),
                fields: BTreeMap::from([
                    ("from".into(), Value::String("person/test".into())),
                    ("to".into(), Value::String(to.into())),
                    ("content".into(), Value::String(id.into())),
                    ("status".into(), Value::String("sent".into())),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some(format!("mailbox-page-{id}")),
            })
            .unwrap();
    }
    let through = store.index().unwrap();
    let (first, cursor) = store
        .messages_page(Some("agent/worker"), true, None, through, 1)
        .unwrap();
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].subject, "message/first");
    let (second, next) = store
        .messages_page(Some("agent/worker"), true, cursor, through, 1)
        .unwrap();
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].subject, "message/second");
    assert!(next.is_none());
    assert_eq!(store.messages(Some("agent/other"), true).unwrap().len(), 1);

    for (kind, status) in [
        ("message.staged", "staged"),
        ("message.delivered", "delivered"),
        ("message.read", "read"),
        ("message.closed", "closed"),
    ] {
        store
            .append_claim(&ClaimInput {
                subject: "message/first".into(),
                kind: kind.into(),
                actor: Some("agent/worker".into()),
                fields: BTreeMap::from([("status".into(), Value::String(status.into()))]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some(format!("mailbox-first-{status}")),
            })
            .unwrap();
    }
    let (open, next) = store
        .messages_page(Some("agent/worker"), false, None, store.index().unwrap(), 1)
        .unwrap();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].subject, "message/second");
    assert!(next.is_none());
    assert_eq!(
        store.message("message/first").unwrap().unwrap().status,
        "closed"
    );
    let (closed, _) = store
        .messages_page(Some("agent/worker"), true, None, store.index().unwrap(), 1)
        .unwrap();
    assert_eq!(closed[0].status, "closed");
}

#[test]
fn reconciler_work_mailbox_keeps_closed_attempts_without_unrelated_messages() {
    let store = Store::open_memory("node").unwrap();
    for (id, to, content, tags) in [
        (
            "wake-closed",
            "agent/worker",
            "ready",
            vec!["st3-work:step-run/one@1@1@worker"],
        ),
        (
            "ordinary",
            "agent/worker",
            "mentions st3-work: but has no work tag",
            vec![],
        ),
        (
            "wake-open",
            "agent/worker",
            "ready again",
            vec!["st3-work:step-run/two@1@1@worker"],
        ),
        (
            "other-agent",
            "agent/other",
            "ready",
            vec!["st3-work:step-run/other@1@1@other"],
        ),
    ] {
        store
            .append_claim(&ClaimInput {
                subject: format!("message/{id}"),
                kind: "message.sent".into(),
                actor: Some("daemon/runtime".into()),
                fields: BTreeMap::from([
                    ("from".into(), Value::String("daemon/runtime".into())),
                    ("to".into(), Value::String(to.into())),
                    ("content".into(), Value::String(content.into())),
                    ("status".into(), Value::String("sent".into())),
                    (
                        "tags".into(),
                        Value::Array(
                            tags.into_iter()
                                .map(|tag| Value::String(tag.into()))
                                .collect(),
                        ),
                    ),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some(format!("work-mailbox-{id}")),
            })
            .unwrap();
    }
    for (kind, status) in [
        ("message.staged", "staged"),
        ("message.delivered", "delivered"),
        ("message.read", "read"),
        ("message.closed", "closed"),
    ] {
        store
            .append_claim(&ClaimInput {
                subject: "message/wake-closed".into(),
                kind: kind.into(),
                actor: Some("agent/worker".into()),
                fields: BTreeMap::from([("status".into(), Value::String(status.into()))]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some(format!("closed-work-mailbox-{status}")),
            })
            .unwrap();
    }

    let expected = store
        .messages(Some("agent/worker"), true)
        .unwrap()
        .into_iter()
        .filter(|message| message.tags.iter().any(|tag| tag.starts_with("st3-work:")))
        .map(|message| (message.subject, message.status))
        .collect::<Vec<_>>();
    let actual = store
        .work_wake_messages_for_reconcile("agent/worker")
        .unwrap()
        .into_iter()
        .map(|message| (message.subject, message.status))
        .collect::<Vec<_>>();
    assert_eq!(actual, expected);
    assert_eq!(actual.len(), 2);
    assert_eq!(actual[0].1, "closed");
    assert_eq!(actual[1].1, "sent");
}

#[test]
fn concurrent_message_close_converges_to_one_claim() {
    let store = Arc::new(Store::open_memory("node").unwrap());
    let subject = "message/concurrent-close";
    for (kind, status, key) in [
        ("message.sent", "sent", "concurrent-sent"),
        ("message.delivered", "delivered", "concurrent-delivered"),
        ("message.read", "read", "concurrent-read"),
    ] {
        store
            .append_claim(&ClaimInput {
                subject: subject.into(),
                kind: kind.into(),
                actor: Some("agent/worker".into()),
                fields: BTreeMap::from([("status".into(), Value::String(status.into()))]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some(key.into()),
            })
            .unwrap();
    }
    let barrier = Arc::new(std::sync::Barrier::new(8));
    let threads = (0..8)
        .map(|index| {
            let store = store.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                store.append_claim(&ClaimInput {
                    subject: subject.into(),
                    kind: "message.closed".into(),
                    actor: Some("agent/worker".into()),
                    fields: BTreeMap::from([("status".into(), Value::String("closed".into()))]),
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: Some(format!("concurrent-close-{index}")),
                })
            })
        })
        .collect::<Vec<_>>();
    let records = threads
        .into_iter()
        .map(|thread| thread.join().unwrap().unwrap())
        .collect::<Vec<_>>();
    assert!(records.iter().all(|record| record.id == records[0].id));
    assert_eq!(
        store
            .claims_for(subject, Some("message.closed"))
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn desired_messages_use_canonical_agent_parties_in_views() {
    let store = Store::open_memory("node").unwrap();
    let intent = crate::graph::parse_test_intent(
        r#"
version 2

  agent "mix.sup" {
    workspace "/work"
    command "sleep 60"
    restart "never"
  }
  message "kickoff" {
    from "requester"
    to "mix.sup"
    content "work"
  }

"#,
        "local",
    )
    .unwrap();
    let mission = store
        .mission(
            &intent,
            IntentInput {
                kdl: "message".into(),
                source_name: None,
            },
        )
        .unwrap();
    store
        .apply(&intent, &mission.subject_tokens, "message")
        .unwrap();

    let messages = store.messages(Some("agent/mix.sup"), true).unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].from, "requester");
    assert_eq!(messages[0].to, "agent/mix.sup");
    let (page, next) = store
        .messages_page(
            Some("agent/mix.sup"),
            false,
            None,
            store.index().unwrap(),
            1,
        )
        .unwrap();
    assert_eq!(page.len(), 1);
    assert_eq!(page[0].subject, messages[0].subject);
    assert!(next.is_none());
}

#[test]
fn an_old_nonempty_database_is_rejected() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.sqlite3");
    let connection = Connection::open(&path).unwrap();
    connection
        .execute("CREATE TABLE desired(subject TEXT PRIMARY KEY)", [])
        .unwrap();
    drop(connection);

    let error = Store::open(&path, "node")
        .err()
        .expect("the old schema must be rejected");
    assert!(error.to_string().contains("unsupported st schema"));
}

#[test]
fn schema_version_nine_requires_fresh_state_for_version_ten() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.sqlite3");
    let connection = Connection::open(&path).unwrap();
    connection
        .execute("CREATE TABLE marker(value TEXT)", [])
        .unwrap();
    connection.pragma_update(None, "user_version", 9).unwrap();
    drop(connection);

    let error = Store::open(&path, "node")
        .err()
        .expect("schema version 9 must be rejected");
    assert!(error.to_string().contains("unsupported st schema"));
}

#[test]
fn schema_version_ten_upgrades_additively_for_mission_deadlines() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.sqlite3");
    drop(Store::open(&path, "node").unwrap());

    let connection = Connection::open(&path).unwrap();
    connection
        .execute("DROP TABLE mission_run_deadlines", [])
        .unwrap();
    connection.pragma_update(None, "user_version", 10).unwrap();
    drop(connection);

    drop(Store::open(&path, "node").unwrap());
    let connection = Connection::open(&path).unwrap();
    assert_eq!(
        connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, u32>(0))
            .unwrap(),
        st3_schema::STORAGE_VERSION
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='mission_run_deadlines'",
                [],
                |row| row.get::<_, u32>(0),
            )
            .unwrap(),
        1
    );
}

#[test]
fn schema_version_eleven_removes_writer_sequence_and_pending_proposal_indexes() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.sqlite3");
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch(
            "PRAGMA foreign_keys=ON;
                 CREATE TABLE batches (
                   id TEXT PRIMARY KEY,
                   origin TEXT NOT NULL,
                   replica_sequence INTEGER NOT NULL,
                   previous_hash TEXT,
                   hash TEXT NOT NULL,
                   accepted_at_unix_ms TEXT NOT NULL,
                   UNIQUE(origin, replica_sequence)
                 );
                 CREATE TABLE claims (
                   store_index INTEGER PRIMARY KEY AUTOINCREMENT,
                   id TEXT NOT NULL UNIQUE,
                   batch_id TEXT NOT NULL REFERENCES batches(id),
                   subject TEXT NOT NULL,
                   kind TEXT NOT NULL,
                   origin TEXT NOT NULL,
                   actor TEXT,
                   body TEXT NOT NULL,
                   predecessors TEXT NOT NULL,
                   accepted_at_unix_ms TEXT NOT NULL
                 );
                 PRAGMA user_version=11;",
        )
        .unwrap();
    drop(connection);

    let store = Store::open(&path, "node").unwrap();
    let connection = store.connection.lock().unwrap();
    connection
        .execute(
            "INSERT INTO batches VALUES ('a','writer',1,NULL,'a','1')",
            [],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO batches VALUES ('b','writer',1,NULL,'b','1')",
            [],
        )
        .unwrap();
    let unique_indexes: u64 = connection
        .query_row(
            "SELECT COUNT(*) FROM pragma_index_list('batches') WHERE [unique]=1 AND origin='u'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(unique_indexes, 0, "the writer sequence index is not unique");
    assert_eq!(
        connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, u32>(0))
            .unwrap(),
        st3_schema::STORAGE_VERSION
    );
}

#[test]
fn schema_version_twelve_adds_durable_planner_configuration() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.sqlite3");
    drop(Store::open(&path, "node").unwrap());
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch(
            "DROP TRIGGER projection_digest_planning_sessions_insert;
                 DROP TRIGGER projection_digest_planning_sessions_update;
                 DROP TRIGGER projection_digest_planning_sessions_delete;
                 ALTER TABLE planning_sessions DROP COLUMN planner_spec_json;
                 PRAGMA user_version = 12;",
        )
        .unwrap();
    drop(connection);

    let store = Store::open(&path, "node").unwrap();
    let connection = store.connection.lock().unwrap();
    let version: u32 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    let planner_column: u32 = connection
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('planning_sessions') WHERE name='planner_spec_json'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(version, smallclaims::store::CURRENT_SCHEMA_VERSION);
    assert_eq!(planner_column, 1);
}

#[test]
fn the_current_schema_reopens() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.sqlite3");
    drop(Store::open(&path, "node").unwrap());
    Store::open(&path, "node").unwrap();
}

#[test]
fn replication_projection_uses_the_claim_position_index() {
    let store = Store::open_memory("node").unwrap();
    let connection = store.connection.lock().unwrap();
    let mut statement = connection
        .prepare(
            "EXPLAIN QUERY PLAN
                 SELECT claims.id,
                        COALESCE((SELECT MIN(position) FROM replica_records
                                  WHERE replica_records.claim_id=claims.id), 0)
                 FROM claims",
        )
        .unwrap();
    let plan = statement
        .query_map([], |row| row.get::<_, String>(3))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
        .join("\n");
    assert!(
        plan.contains("replica_records_claim"),
        "the projection query must use the claim position index:\n{plan}"
    );
}

#[test]
fn latest_predecessor_seeks_the_newest_time_block_and_preserves_ties() {
    let store = Store::open_memory("alder").unwrap();
    let mut connection = store.connection.write();
    let transaction = connection.transaction().unwrap();
    let mut expected = Vec::new();
    // Arrival order and claim ID deliberately disagree with canonical time and position.
    for (id, time, writer, sequence, batch) in [
        ("old", "99", "z", 9, "old-batch"),
        ("z-first", "100", "a", 1, "a-batch"),
        ("a-second", "100", "a", 1, "a-batch"),
        ("sequence", "100", "a", 2, "a-later"),
        ("writer", "100", "b", 1, "b-batch"),
        ("batch", "100", "b", 1, "z-batch"),
        ("late-arrival-old-time", "99", "z", 10, "older-time"),
    ] {
        transaction.execute(
            "INSERT OR IGNORE INTO batches(id,origin,replica_sequence,hash,accepted_at_unix_ms)
                 VALUES(?1,?2,?3,?1,?4)", params![batch, writer, sequence, time],
        ).unwrap();
        transaction.execute(
            "INSERT INTO claims(id,batch_id,subject,kind,origin,body,predecessors,accepted_at_unix_ms)
                 VALUES(?1,?2,'daemon/alder','daemon.diagnostic',?3,'{}','[]',?4)",
            params![id, batch, writer, time],
        ).unwrap();
        expected.push(canonical::claim_key(&transaction, id).unwrap());
        assert_eq!(
            latest_claim_id_tx(&transaction, "daemon/alder").unwrap(),
            expected.iter().max().map(|key| key.5.clone())
        );
    }
    assert_eq!(
        latest_claim_id_tx(&transaction, "daemon/missing").unwrap(),
        None
    );
    let plan = transaction
        .prepare(&format!(
            "EXPLAIN QUERY PLAN {}",
            canonical_sql(LATEST_CLAIM_QUERY)
        ))
        .unwrap()
        .query_map(["daemon/alder"], |row| row.get::<_, String>(3))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
        .join("\n");
    assert!(
        plan.contains("claims_subject_accepted_index (subject=?)"),
        "{plan}"
    );
    assert!(
        !plan.contains("USE TEMP B-TREE FOR ORDER BY"),
        "history must not be sorted: {plan}"
    );
}

#[test]
fn claims_of_one_kind_use_the_subject_kind_index() {
    let store = Store::open_memory("node").unwrap();
    let connection = store.connection.lock().unwrap();
    let mut statement = connection
        .prepare(&format!(
            "EXPLAIN QUERY PLAN {}",
            claims_for_subject_query(true)
        ))
        .unwrap();
    let plan = statement
        .query_map(params!["agent/example", "harness.diagnostic"], |row| {
            row.get::<_, String>(3)
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
        .join("\n");
    assert!(
        plan.contains("claims_subject_kind_accepted_index (subject=? AND kind=?)")
            || plan.contains("claims_subject_kind_index (subject=? AND kind=?)"),
        "one kind of a subject's claims must not read every claim of the subject:\n{plan}"
    );
    drop(statement);
    drop(connection);

    let mut connection = store.connection.lock().unwrap();
    let transaction = connection.transaction().unwrap();
    for (kind, fields) in (0..20)
        .map(|sequence| {
            (
                "harness.observed",
                json!({"state": "working", "observed_at_ms": sequence}),
            )
        })
        .chain([(
            "harness.diagnostic",
            json!({"code": "provider-capacity", "status": "waiting"}),
        )])
    {
        append_claim_tx(
            &transaction,
            &store.origin,
            "agent/example",
            kind,
            None,
            &json!({ "fields": fields }),
            &[],
            None,
        )
        .unwrap();
    }
    transaction.commit().unwrap();
    drop(connection);
    let diagnostics = store
        .claims_for("agent/example", Some("harness.diagnostic"))
        .unwrap();
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].kind, "harness.diagnostic");
    assert_eq!(store.claims_for("agent/example", None).unwrap().len(), 21);
}

#[test]
fn mailbox_state_read_preserves_ended_diagnostic_work_and_legacy_semantics() {
    let store = Store::open_memory("alder").unwrap();
    let mut connection = store.connection.write();
    let transaction = connection.transaction().unwrap();
    let subject = "agent/example";
    let fence = crate::mailbox::Fence::new(subject, "current", "delivery");
    let mut sequence = 0;
    let mut insert = |kind: &str, fields: Value| {
        sequence += 1;
        let id = format!("claim-{sequence}");
        let time = sequence.to_string();
        transaction
            .execute(
                "INSERT INTO batches(id,origin,replica_sequence,hash,accepted_at_unix_ms)
                 VALUES(?1,'alder',?2,?1,?3)",
                params![id, sequence, time],
            )
            .unwrap();
        transaction.execute(
            "INSERT INTO claims(id,batch_id,subject,kind,origin,actor,body,predecessors,accepted_at_unix_ms)
                 VALUES(?1,?1,?2,?3,'alder',?2,?4,'[]',?5)",
            params![id, subject, kind, json!({"fields": fields}).to_string(), time],
        ).unwrap();
    };
    insert(
        "runtime.observed",
        json!({"status":"running","incarnation_id":"current"}),
    );
    for (kind, fields, expected_ended) in [
        (
            "harness.observed",
            json!({"state":"ended","incarnation_id":"prior"}),
            false,
        ),
        (
            "harness.observed",
            json!({"state":"idle","incarnation_id":"current"}),
            false,
        ),
        (
            "harness.observed",
            json!({"state":"ended","incarnation_id":"current"}),
            true,
        ),
        (
            "harness.observed",
            json!({"driver":"claude","incarnation_id":"current"}),
            true,
        ),
        (
            "harness.diagnostic",
            json!({"code":"provider-auth-expired","incarnation_id":"current"}),
            false,
        ),
        (
            "harness.diagnostic",
            json!({"code":"provider-auth-restored","incarnation_id":"current"}),
            true,
        ),
        (
            "work.progress",
            json!({"claim_incarnation":"current"}),
            false,
        ),
        ("harness.observed", json!({"state":"ended"}), true),
        ("harness.observed", json!({"state":"idle"}), false),
    ] {
        insert(kind, fields);
        let ended = current_harness_at(&transaction, subject, None)
            .unwrap()
            .is_some_and(|harness| harness.state == "ended");
        assert_eq!(ended, expected_ended, "{kind}");
        assert_eq!(
            check_mailbox_incarnation(&transaction, &fence).is_err(),
            ended,
            "{kind}"
        );
    }
}

#[test]
fn subscription_run_lookup_cost_does_not_grow_with_unrelated_deliveries() {
    let store = Store::open_memory("orchid").unwrap();
    let mut connection = store.connection.write();
    let transaction = connection.transaction().unwrap();
    let mut costs = Vec::new();
    for n in 1..=10_000 {
        let id = format!("started-{n}");
        transaction
            .execute(
                "INSERT INTO batches(id,origin,replica_sequence,hash,accepted_at_unix_ms)
                 VALUES(?1,'orchid',1,?1,'1')",
                [&id],
            )
            .unwrap();
        transaction.execute(
            "INSERT INTO claims(id,batch_id,subject,kind,origin,body,predecessors,accepted_at_unix_ms)
                 VALUES(?1,?1,'subscription/reviews','subscription.mission-started','orchid',?2,'[]','1')",
            params![id, json!({"fields":{"mission_run":format!("mission-run/{n}")}}).to_string()],
        ).unwrap();
        if n == 100 || n == 10_000 {
            transaction
                .prepare_cached(SUBSCRIPTION_STARTED_RUN_QUERY)
                .unwrap()
                .reset_status(rusqlite::StatementStatus::VmStep);
            assert!(subscription_started_run_tx(&transaction, "mission-run/1").unwrap());
            assert!(!subscription_started_run_tx(&transaction, "mission-run/authored").unwrap());
            costs.push(
                transaction
                    .prepare_cached(SUBSCRIPTION_STARTED_RUN_QUERY)
                    .unwrap()
                    .get_status(rusqlite::StatementStatus::VmStep),
            );
        }
    }
    assert!(
        costs[0] > 0 && costs[1] <= costs[0] * 2 && costs[1] < 100,
        "positive and absent subscription lookups must seek, not scan history: {costs:?}"
    );
}

#[test]
fn mailbox_fence_cost_does_not_grow_with_sparse_prior_incarnations() {
    let store = Store::open_memory("alder").unwrap();
    let mut connection = store.connection.write();
    let transaction = connection.transaction().unwrap();
    let insert = |id: &str, kind: &str, fields: Value, time: &str| {
        transaction
            .execute(
                "INSERT INTO batches(id,origin,replica_sequence,hash,accepted_at_unix_ms)
                 VALUES(?1,'alder',1,?1,?2)",
                params![id, time],
            )
            .unwrap();
        transaction.execute(
            "INSERT INTO claims(id,batch_id,subject,kind,origin,body,predecessors,accepted_at_unix_ms)
                 VALUES(?1,?1,'agent/example',?2,'alder',?3,'[]',?4)",
            params![id, kind, json!({"fields": fields}).to_string(), time],
        ).unwrap();
    };
    insert(
        "runtime",
        "runtime.observed",
        json!({"status":"running", "incarnation_id":"current"}),
        "20000",
    );
    insert(
        "current",
        "harness.observed",
        json!({"state":"idle", "incarnation_id":"current"}),
        "20001",
    );
    let fence = crate::mailbox::Fence::new("agent/example", "current", "delivery");
    // Count the actual statements used by the fence, including the previous display fold.
    // Cached statement reset does not reset SQLite's VM step counter.
    let queries = [
        newest_mailbox_harness_state_query(),
        newest_claims_of_kind_query("claims.id, claims.body", "harness.observed"),
        newest_claims_of_kind_query(
            "claims.id, claims.store_index, claims.body, claims.accepted_at_unix_ms",
            "harness.observed",
        ),
    ];
    let mut costs = Vec::new();
    for n in 1..=10_000 {
        insert(
            &format!("old-{n}"),
            "harness.observed",
            json!({"state":"working", "incarnation_id":"prior"}),
            &n.to_string(),
        );
        if n == 100 || n == 10_000 {
            for query in &queries {
                transaction
                    .prepare_cached(query)
                    .unwrap()
                    .reset_status(rusqlite::StatementStatus::VmStep);
            }
            check_mailbox_incarnation(&transaction, &fence).unwrap();
            let cost: i32 = queries
                .iter()
                .map(|query| {
                    transaction
                        .prepare_cached(query)
                        .unwrap()
                        .get_status(rusqlite::StatementStatus::VmStep)
                })
                .sum();
            costs.push(cost);
        }
    }
    assert!(
        costs[0] > 0 && costs[1] <= costs[0] * 2 && costs[1] < 1_000,
        "a live fence must read its state, not its optional display history: {costs:?}"
    );
}

/// The reads behind the session list, a mission detail and the missions tree seek or walk an
/// index whose part they read is what they show: one incarnation's claims, the newest
/// observations, the open runs and steps. None reads or sorts every claim of a kind, every
/// claim of a subject, or every step and run the store holds.
/// A mailbox stream asks on every graph change whether its seat has a newer local observation
/// (`mailbox_changed_since`). That seeks the seat's newest observations; by subject and kind
/// it read every observation the seat ever had, 1.3 ms for a seat with 36,000 of them, about
/// 160 times a second on a busy member.
#[test]
fn a_mailbox_stream_seeks_its_seats_newer_local_observations() {
    let store = Store::open_memory("node").unwrap();
    let connection = store.readers.get();
    let plan = connection
        .prepare(
            "EXPLAIN QUERY PLAN SELECT EXISTS(SELECT 1 FROM local_observations
                 WHERE subject=?1 AND id>?2)",
        )
        .unwrap()
        .query_map(params!["agent/example", 0], |row| row.get::<_, String>(3))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
        .join("; ");
    assert!(plan.contains("(subject=? AND id>?)"), "{plan}");
}

/// Foreign keys are on, so deleting or rekeying a parent row looks up every child that names
/// it. A child column without an index made each lookup read the whole child table: deleting
/// a claim read every operation, and a checkpoint trim held the writer for an hour.
#[test]
fn every_foreign_key_column_leads_an_index() {
    let store = Store::open_memory("node").unwrap();
    let connection = store.readers.get();
    let tables = connection
        .prepare("SELECT name FROM sqlite_master WHERE type='table'")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    let mut unindexed = Vec::new();
    for table in tables {
        let references = connection
            .prepare(&format!("PRAGMA foreign_key_list(\"{table}\")"))
            .unwrap()
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>("from")?,
                    row.get::<_, String>("table")?,
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        for (column, parent) in references {
            let leading = connection
                .prepare(&format!("PRAGMA index_list(\"{table}\")"))
                .unwrap()
                .query_map([], |row| row.get::<_, String>("name"))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
                .into_iter()
                .any(|index| {
                    connection
                        .query_row(
                            &format!(
                                "SELECT name FROM pragma_index_info('{index}') WHERE seqno=0"
                            ),
                            [],
                            |row| row.get::<_, String>(0),
                        )
                        .is_ok_and(|first| first == column)
                });
            if !leading {
                unindexed.push(format!("{table}.{column} -> {parent}"));
            }
        }
    }
    assert!(
        unindexed.is_empty(),
        "unindexed foreign keys: {unindexed:?}"
    );
}

#[test]
fn claude_attachment_lookup_cost_ignores_hook_runtime_and_predecessor_history() {
    let store = Store::open_memory("node").unwrap();
    let append = |kind: &str, incarnation: &str, code: &str, key: &str| {
        let fields = match kind {
            "harness.diagnostic" => json!({"incarnation_id":incarnation,
                "code":code,"status":"blocked","severity":"warning","reason":"cost fixture"}),
            "harness.observed" => json!({"incarnation_id":incarnation,"state":"idle","driver":"claude"}),
            "runtime.observed" => json!({"incarnation_id":incarnation,"status":"running",
                "runtime_id":"grove/cedar","reachability":"local"}),
            _ => unreachable!(),
        };
        store.append_claim(&ClaimInput {
            subject: "agent/grove/cedar".into(),
            kind: kind.into(), actor: None,
            fields: serde_json::from_value(fields).unwrap(),
            evidence: Vec::new(), expected_subject: None,
            idempotency_key: Some(key.into()),
        }).unwrap();
    };
    append("harness.diagnostic", "current", "claude-channel-unattached", "attach");
    let work = || {
        let connection = store.connection.lock().unwrap();
        let mut statement = connection.prepare(&claude_attachment_query()).unwrap();
        let claim: String = statement.query_row(
            params!["agent/grove/cedar", i64::MAX, "current"], |row| row.get(0),
        ).unwrap();
        (claim, statement.get_status(rusqlite::StatementStatus::VmStep))
    };
    let before = work();
    for index in 0..128 {
        for (kind, incarnation, code) in [
            ("harness.observed", "current", ""),
            ("runtime.observed", "current", ""),
            ("harness.diagnostic", "current", "native-delivery-recovered"),
            ("harness.diagnostic", "previous", "claude-channel-attached"),
        ] {
            append(kind, incarnation, code, &format!("history:{kind}:{incarnation}:{index}"));
        }
    }
    let after = work();
    assert_eq!(after.0, before.0);
    assert!(after.1 <= before.1 + 20, "attachment lookup grew with unrelated history: {before:?} -> {after:?}");
}

#[test]
fn unnamed_harness_seek_preserves_legacy_candidates_and_snapshot_cut() {
    let store = Store::open_memory("legacy-seek").unwrap();
    let mut connection = store.connection.write();
    let transaction = connection.transaction().unwrap();
    transaction
        .execute(
            "INSERT INTO batches(id,origin,replica_sequence,hash,accepted_at_unix_ms)
             VALUES ('legacy-batch','legacy-seek',1,'synthetic','1')",
            [],
        )
        .unwrap();
    let bodies = [
        json!({"fields":{"state":"idle"}}),
        json!({"state":"working","incarnation_id":null}),
        json!({"fields":{"state":"idle","incarnation_id":7}}),
        json!({"fields":{"state":"idle","incarnation_id":false}}),
        json!({"fields":{"state":"idle","incarnation_id":"current"}}),
        json!({"state":"idle","incarnation_id":"current"}),
        json!({"fields":{"state":"idle","incarnation_id":[7]}}),
        json!({"fields":null,"state":"idle","incarnation_id":"current"}),
        json!({"fields":{"reason":"sparse legacy fields"}}),
    ];
    for (position, body) in bodies.iter().enumerate() {
        let index = position + 1;
        transaction.execute(
            "INSERT INTO claims(store_index,id,batch_id,subject,kind,origin,body,predecessors,accepted_at_unix_ms)
                 VALUES (?1,?2,'legacy-batch','agent/legacy-seek','harness.observed','legacy-seek',?3,'[]',?4)",
            params![index as i64,format!("legacy-{index}"),body.to_string(),index.to_string()],
        ).unwrap();
    }
    transaction.commit().unwrap();
    let read = |sql: &str, cut: i64, since: &str| {
        connection
            .prepare(sql)
            .unwrap()
            .query_map(params!["agent/legacy-seek", cut, since], |row| {
                row.get::<_, String>(0)
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    };
    let query = harness_observations_without_incarnation_query();
    // The retained pre-change SQL is an independent candidate oracle; only its index differs.
    let oracle = query.replace(
        "claims_harness_unnamed_accepted_index",
        "claims_subject_kind_accepted_index",
    );
    for (cut, since, expected) in [
        (
            9,
            "1",
            vec![
                "legacy-9", "legacy-8", "legacy-4", "legacy-3", "legacy-2", "legacy-1",
            ],
        ),
        (4, "1", vec!["legacy-4", "legacy-3", "legacy-2", "legacy-1"]),
        (9, "4", vec!["legacy-9", "legacy-8", "legacy-4"]),
        (9, "10", vec![]),
    ] {
        assert_eq!(read(&query, cut, since), expected);
        assert_eq!(read(&query, cut, since), read(&oracle, cut, since));
    }
    // The partial index also follows updates/deletes; repaired observation bodies cannot
    // leave a stale legacy candidate, and becoming unnamed must make a new candidate.
    connection.execute("UPDATE claims SET body=json_set(body,'$.fields.incarnation_id','current') WHERE id='legacy-3'",[]).unwrap();
    connection.execute("UPDATE claims SET body=json_remove(body,'$.fields.incarnation_id') WHERE id='legacy-5'",[]).unwrap();
    connection
        .execute("DELETE FROM claims WHERE id='legacy-2'", [])
        .unwrap();
    assert_eq!(
        read(&query, 9, "1"),
        vec!["legacy-9", "legacy-8", "legacy-5", "legacy-4", "legacy-1"]
    );
    assert_eq!(read(&query, 9, "1"), read(&oracle, 9, "1"));
}

#[test]
fn unnamed_harness_seek_cost_ignores_named_heartbeat_growth() {
    let measure = |named: usize| {
        let store = Store::open_memory("legacy-cost").unwrap();
        let mut connection = store.connection.write();
        let transaction = connection.transaction().unwrap();
        transaction
            .execute(
                "INSERT INTO batches(id,origin,replica_sequence,hash,accepted_at_unix_ms)
                VALUES ('cost-batch','legacy-cost',1,'synthetic','1')",
                [],
            )
            .unwrap();
        for index in 1..=named {
            let body = json!({"fields":{"incarnation_id":"current","state":"idle","status_transition":false}});
            transaction.execute("INSERT INTO claims(store_index,id,batch_id,subject,kind,origin,body,predecessors,accepted_at_unix_ms)
                    VALUES (?1,?2,'cost-batch','agent/legacy-cost','harness.observed','legacy-cost',?3,'[]',?4)",
                params![index as i64,format!("named-{index}"),body.to_string(),index.to_string()]).unwrap();
        }
        transaction.execute("INSERT INTO claims(store_index,id,batch_id,subject,kind,origin,body,predecessors,accepted_at_unix_ms)
                VALUES (100000,'legacy-tail','cost-batch','agent/legacy-cost','harness.observed','legacy-cost',?1,'[]','100000')",[json!({"fields":{"state":"idle"}}).to_string()]).unwrap();
        // Native claims have a recorded canonical position. Without it, the unchanged
        // legacy-position fallback counts this synthetic batch's preceding rows, a
        // separate ordering cost from selecting unnamed candidates.
        transaction.execute(
            "INSERT INTO replica_records(record_ref,writer,sequence,envelope_hash,position,raw,state,claim_id,updated_at_unix_ms)
                 VALUES ('cost-record','legacy-cost',1,'synthetic',0,X'', 'valid','legacy-tail','1')",
            [],
        ).unwrap();
        transaction.commit().unwrap();
        let query = harness_observations_without_incarnation_query();
        let plans = connection
            .prepare(&format!("EXPLAIN QUERY PLAN {query}"))
            .unwrap()
            .query_map(params!["agent/legacy-cost", 100000, "1"], |row| {
                row.get::<_, String>(3)
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert!(
            plans
                .iter()
                .any(|p| p.contains("claims_harness_unnamed_accepted_index")),
            "{plans:?}"
        );
        let work = |sql: &str| {
            let mut statement = connection.prepare(sql).unwrap();
            let ids = statement
                .query_map(params!["agent/legacy-cost", 100000, "1"], |row| {
                    row.get::<_, String>(0)
                })
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            assert_eq!(ids, vec!["legacy-tail"]);
            statement.get_status(rusqlite::StatementStatus::VmStep)
        };
        let new = work(&query);
        let old = work(&query.replace(
            "claims_harness_unnamed_accepted_index",
            "claims_subject_kind_accepted_index",
        ));
        println!("unnamed seek: named={named} new_vm_steps={new} old_vm_steps={old}");
        (new, old)
    };
    let small = measure(128);
    let large = measure(4096);
    assert!(
        small.0 > 0 && large.0 <= small.0 + 20,
        "{small:?} -> {large:?}"
    );
    assert!(
        large.1 > small.1 * 8,
        "the retained old query must demonstrate the scan: {small:?} -> {large:?}"
    );
}

#[test]
fn person_reads_seek_what_they_show() {
    let store = Store::open_memory("node").unwrap();
    let connection = store.connection.lock().unwrap();
    let plan = |sql: &str| {
        let mut statement = connection
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .unwrap();
        let parameters = vec!["agent/example"; statement.parameter_count()];
        statement
            .query_map(rusqlite::params_from_iter(parameters), |row| {
                row.get::<_, String>(3)
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
            .join("\n")
    };
    for order in ["ASC", "DESC"] {
        let plan = plan(&incarnation_claim_time_query(order));
        assert!(
            plan.contains("claims_incarnation_accepted_index (subject=? AND <expr>=?)")
                && !plan.contains("TEMP B-TREE"),
            "a session's {order} date must be one seek into its incarnation's claims:\n{plan}"
        );
    }
    let agents = plan(AGENT_STATUS_INDEX_QUERY);
    assert!(
        agents.contains("USING INTEGER PRIMARY KEY") && !agents.contains("MULTI-INDEX OR"),
        "the newest agent claim must be a walk back from the snapshot:\n{agents}"
    );
    let harness = plan(&newest_claims_of_kind_query(
        "claims.id",
        "harness.observed",
    ));
    assert!(
        harness.contains("claims_subject_kind_accepted_index (subject=? AND kind=?)")
            && !harness.contains("TEMP B-TREE FOR ORDER BY"),
        "a seat's newest observation must not sort every observation it made:\n{harness}"
    );
    let latest = plan(&latest_claim_of_kind_query());
    assert!(
        latest.contains("claims_subject_kind_accepted_index (subject=? AND kind=?)")
            && !latest.contains("TEMP B-TREE FOR ORDER BY"),
        "a subject's newest claim of a kind must not sort every claim of the kind:\n{latest}"
    );
    let open_steps = plan(&work_at_snapshot_query(true));
    assert!(
        open_steps.contains("step_runs_open_index"),
        "a seat queue must read the open steps, not every step:\n{open_steps}"
    );
    let selected_steps = plan(&selected_work_at_snapshot_query(false));
    assert!(
        !selected_steps.contains("SCAN step_runs\n"),
        "the published work list refolds its changed steps by subject, not every step:\n{selected_steps}"
    );
    for open_only in [false, true] {
        let seat_steps = plan(&seat_work_at_snapshot_query(open_only));
        assert!(
            seat_steps.contains("step_runs_assignee_index (assignee=?)")
                && seat_steps.contains("step_runs_lease_index (lease_owner=?)")
                && seat_steps.contains("step_runs_available_index")
                && !seat_steps.contains("SCAN step_runs\n"),
            "a seat's steps must be found by seat, not by reading every step:\n{seat_steps}"
        );
    }
    let joins = plan(&seat_queue_joins_query());
    assert!(
        joins.contains("SCAN mission_runs USING INDEX mission_runs_open_index")
            && joins.contains("step_runs_run_index (run_id=?)"),
        "a seat queue must read the open runs' steps, not every assigned step:\n{joins}"
    );
    let runs = plan(&open_mission_run_headers_query());
    assert!(
        runs.contains("mission_runs_open_index"),
        "the missions tree must read the open runs, not every run:\n{runs}"
    );
}

/// A claim names its incarnation in its fields, or, when it has none, in its body, exactly as
/// a view that reads the claim finds it; the index expression agrees with that reading.
#[test]
fn the_incarnation_index_reads_a_claim_as_a_view_does() {
    let store = Store::open_memory("node").unwrap();
    let connection = store.connection.lock().unwrap();
    for body in [
        json!({"fields": {"incarnation_id": "current"}}),
        json!({"fields": {"incarnation_id": "current"}, "incarnation_id": "other"}),
        json!({"fields": {"state": "idle"}, "incarnation_id": "other"}),
        json!({"fields": null, "incarnation_id": "other"}),
        json!({"incarnation_id": "current"}),
        json!({"incarnation_id": 7}),
        json!({"state": "idle"}),
        json!(["current"]),
    ] {
        let viewed = body
            .get("fields")
            .unwrap_or(&body)
            .get("incarnation_id")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let indexed = connection
            .query_row(
                &format!("SELECT {INCARNATION_OF_CLAIM} FROM (SELECT ?1 AS body)"),
                [body.to_string()],
                |row| row.get::<_, rusqlite::types::Value>(0),
            )
            .unwrap();
        let indexed = match indexed {
            rusqlite::types::Value::Text(text) => Some(text),
            _ => None,
        };
        assert_eq!(indexed, viewed, "{body}");
    }
}

/// A session is dated by the first and last acceptance times of the claims that name its
/// incarnation, or its runtime when it has none, as reading every claim of the subject and
/// keeping those dated it; `through_claim` keeps only the claims up to it.
#[test]
fn a_runtime_claim_span_is_its_first_and_last_claim() {
    let store = Store::open_memory("node").unwrap();
    let subject = "agent/example";
    {
        let mut connection = store.connection.lock().unwrap();
        let transaction = connection.transaction().unwrap();
        for (kind, fields) in [
            (
                "runtime.observed",
                json!({"status": "running", "runtime_id": "runtime-a", "incarnation_id": "first"}),
            ),
            (
                "harness.observed",
                json!({"state": "working", "incarnation_id": "first"}),
            ),
            (
                "runtime.observed",
                json!({"status": "running", "runtime_id": "runtime-b", "incarnation_id": "second"}),
            ),
            (
                "harness.observed",
                json!({"state": "idle", "incarnation_id": "second"}),
            ),
            ("harness.observed", json!({"state": "working"})),
            (
                "runtime.observed",
                json!({"status": "running", "runtime_id": "runtime-c"}),
            ),
            (
                "harness.observed",
                json!({"state": "idle", "incarnation_id": "second"}),
            ),
            (
                "runtime.observed",
                json!({"status": "exited", "runtime_id": "runtime-c"}),
            ),
        ] {
            append_claim_tx(
                &transaction,
                &store.origin,
                subject,
                kind,
                None,
                &json!({ "fields": fields }),
                &[],
                None,
            )
            .unwrap();
        }
        // Acceptance times out of store order, as claims replicated from a peer arrive.
        transaction
            .execute(
                "UPDATE claims SET accepted_at_unix_ms=CAST(1000 + (store_index * 37) % 97 AS TEXT)
                     WHERE subject=?1",
                [subject],
            )
            .unwrap();
        transaction.commit().unwrap();
    }
    let index = store.index().unwrap();
    let all = store
        .claims_page(Some(subject), None, 0, None, false, 100)
        .unwrap()
        .claims;
    for window in [3, 6, all.len()] {
        let read = &all[..window];
        let through_claim = (window < all.len()).then(|| read[window - 1].id.as_str());
        for (incarnation, runtime) in [
            (Some("first"), None),
            (Some("second"), Some("runtime-b")),
            (Some("missing"), Some("runtime-a")),
            (None, Some("runtime-c")),
            (None, Some("runtime-a")),
            (None, None),
        ] {
            let times = read
                .iter()
                .filter(|claim| {
                    let fields = claim.body.get("fields").unwrap_or(&claim.body);
                    let named = |name: &str, expected: Option<&str>| {
                        expected.is_some_and(|expected| {
                            fields.get(name).and_then(Value::as_str) == Some(expected)
                        })
                    };
                    named("incarnation_id", incarnation)
                        || (incarnation.is_none() && named("runtime_id", runtime))
                })
                .map(|claim| claim.accepted_at_unix_ms)
                .collect::<Vec<_>>();
            let expected = times.iter().min().copied().zip(times.iter().max().copied());
            assert_eq!(
                store
                    .runtime_claim_span_at(subject, incarnation, runtime, index, through_claim)
                    .unwrap(),
                expected,
                "{incarnation:?} {runtime:?} through {window}"
            );
        }
    }
}

/// A mission's launches are the ones every launch lists for it, named with or without its
/// `mission/` prefix, in the same order, read without every other mission's launches.
#[test]
fn a_missions_launches_are_read_by_mission() {
    let store = Store::open_memory("node").unwrap();
    for (id, mission) in [
        ("one", "example/alpha"),
        ("two", "mission/example/alpha"),
        ("three", "example/beta"),
        ("four", "example/alpha-two"),
    ] {
        store
            .create_planning_session(
                id,
                mission,
                "doc/request",
                "/tmp",
                "person/operator",
                "codex",
                &crate::model::PlannerSpec::default(),
                None,
                None,
            )
            .unwrap();
    }
    for mission in [
        "mission/example/alpha",
        "example/alpha",
        "mission/mission/example/alpha",
        "mission/example/beta",
        "mission/example/missing",
    ] {
        let listed = store
            .planning_sessions(true)
            .unwrap()
            .into_iter()
            .filter(|session| {
                session.mission == mission
                    || format!("mission/{}", session.mission) == mission
                    || session.mission == mission.trim_start_matches("mission/")
            })
            .map(|session| session.id)
            .collect::<Vec<_>>();
        let read = store
            .planning_sessions_for_mission(mission)
            .unwrap()
            .into_iter()
            .map(|session| session.id)
            .collect::<Vec<_>>();
        assert_eq!(read, listed, "{mission}");
    }
}

#[test]
fn owned_subject_queries_look_owners_up_by_primary_key() {
    let store = Store::open_memory("node").unwrap();
    let connection = store.connection.lock().unwrap();
    for (name, query) in [
        ("terminal owned runtimes", TERMINAL_OWNED_RUNTIME_SUBJECTS),
        ("retired owned intake", RETIRED_OWNED_INTAKE_SUBJECTS),
    ] {
        let mut statement = connection
            .prepare(&format!("EXPLAIN QUERY PLAN {query}"))
            .unwrap();
        let plan = statement
            .query_map([], |row| row.get::<_, String>(3))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert!(
            !plan
                .iter()
                .any(|step| step.starts_with("SCAN owner")
                    || step.starts_with("SCAN generation")),
            "{name} must not scan owners for each desired row:\n{}",
            plan.join("\n")
        );
        assert!(
            plan.iter()
                .any(|step| step.starts_with("SEARCH owner USING INDEX")),
            "{name} must look its owner run up by primary key:\n{}",
            plan.join("\n")
        );
    }
}

#[test]
fn replication_inventory_uses_the_batch_indexes() {
    let store = Store::open_memory("node").unwrap();
    let connection = store.connection.lock().unwrap();
    let plan = |query: &str| {
        let mut statement = connection.prepare(query).unwrap();
        statement
            .query_map([], |row| row.get::<_, String>(3))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
            .join("\n")
    };
    let missing_envelopes = plan(
        "EXPLAIN QUERY PLAN
             SELECT id FROM batches
             WHERE NOT EXISTS (
                 SELECT 1 FROM replica_envelopes
                 WHERE replica_envelopes.batch_id=batches.id
             )",
    );
    assert!(
        missing_envelopes.contains("replica_envelopes_batch"),
        "the inventory seed query must use the envelope batch index:\n{missing_envelopes}"
    );
    let incremental_seed = plan(
        "EXPLAIN QUERY PLAN
             SELECT id FROM batches
             WHERE batches.rowid>=1
               AND NOT EXISTS (
                   SELECT 1 FROM replica_envelopes
                   WHERE replica_envelopes.batch_id=batches.id
               )
             ORDER BY batches.rowid",
    );
    assert!(
        incremental_seed.contains("USING INTEGER PRIMARY KEY"),
        "an incremental seed must range-scan only new batches:\n{incremental_seed}"
    );
    let batch_claims = plan(
        "EXPLAIN QUERY PLAN
             SELECT id FROM claims WHERE batch_id='batch/example' ORDER BY store_index",
    );
    assert!(
        batch_claims.contains("claims_batch_index"),
        "the inventory seed query must use the claim batch index:\n{batch_claims}"
    );
}

/// A replicated harness observation. Each one is a new transition: a repeat that changed
/// only `observed_at_ms` would stay in the local observation log.
fn observe_harness(store: &Store, observed_at_ms: u64) {
    store
        .append_claim(&ClaimInput {
            subject: "agent/example".into(),
            kind: "harness.observed".into(),
            actor: Some("agent/example".into()),
            fields: BTreeMap::from([
                ("state".into(), json!("working")),
                ("observed_at_ms".into(), json!(observed_at_ms)),
                ("transition_sequence".into(), json!(observed_at_ms)),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
}

#[test]
fn a_local_write_rehashes_only_its_own_inventory_range() {
    let store = Store::open_memory("example-linux").unwrap();
    for observed_at_ms in 0..600 {
        observe_harness(&store, observed_at_ms);
    }
    let before = store.replication_snapshot().unwrap();
    assert!(before.inventory.envelopes.len() >= 600);
    INVENTORY_IDENTITIES_HASHED.with(|hashed| hashed.set(0));

    observe_harness(&store, 600);
    let after = store.replication_snapshot().unwrap();
    let hashed = INVENTORY_IDENTITIES_HASHED.with(std::cell::Cell::get);
    assert!(
        hashed <= REPLICATION_BUCKET_WIDTH as usize,
        "one new envelope re-hashed {hashed} of {} identities",
        after.inventory.envelopes.len()
    );
    let public = after.inventory.public();
    assert_eq!(
        after.inventory.digest,
        replication_inventory_digest(&public.envelopes)
    );
    let connection = store.connection.lock().unwrap();
    let (full, _) = full_compact_replication_inventory(&connection).unwrap();
    assert_eq!(public.envelopes, full.public().envelopes);
    assert_eq!(after.inventory.digest, full.digest);
}

proptest! {
    #[test]
    fn a_resumed_inventory_digest_matches_a_full_pass(
        initial in proptest::collection::vec((0usize..3, 1u64..2000), 0..300),
        rounds in proptest::collection::vec(
            proptest::collection::vec((0usize..3, 1u64..2000), 1..20),
            1..6,
        ),
    ) {
        let writers = ["ExampleMac", "fleet-node", "example-linux"];
        // Some hashes are not SHA-256 hex, so the digest also covers verbatim hashes.
        let identity = |(writer, sequence): (usize, u64)| ReplicaEnvelopeId {
            writer: writers[writer].into(),
            sequence,
            hash: if sequence % 7 == 0 {
                format!("irregular-{sequence}")
            } else {
                format!("{sequence:064x}")
            },
        };
        let mut all = initial.into_iter().map(identity).collect::<BTreeSet<_>>();
        let mut inventory = CompactReplicationInventory::from_sorted(all.iter().cloned());
        let mut buckets = inventory.buckets();
        let mut prefixes = Vec::new();
        inventory.resume_digest(&buckets, &mut prefixes, 0);
        prop_assert_eq!(
            &inventory.digest,
            &replication_inventory_digest(&all.iter().cloned().collect::<Vec<_>>())
        );
        for round in rounds {
            let before = buckets.clone();
            for identity in round.into_iter().map(identity) {
                if all.insert(identity.clone()) {
                    inventory.insert(identity);
                }
            }
            buckets = inventory.buckets();
            let resume_from = buckets
                .iter()
                .zip(&before)
                .position(|(after, before)| after != before)
                .unwrap_or(before.len().min(buckets.len()));
            inventory.resume_digest(&buckets, &mut prefixes, resume_from);
            prop_assert_eq!(
                &inventory.digest,
                &replication_inventory_digest(&all.iter().cloned().collect::<Vec<_>>())
            );
        }
    }
}

#[test]
fn replication_reuses_legacy_digest_while_complete_source_digest_changes() {
    let store = Store::open_memory("example-linux").unwrap();
    observe_harness(&store, 0);
    let first = store.replication_snapshot().unwrap();
    GRAPH_DIGESTS_COMPUTED.with(|computed| computed.set(0));
    observe_harness(&store, 1);
    let second = store.replication_snapshot().unwrap();
    assert_ne!(first.store_index, second.store_index);
    assert_eq!(
        GRAPH_DIGESTS_COMPUTED.with(std::cell::Cell::get),
        0,
        "a harness observation must not recompute the graph digest"
    );
    assert_eq!(second.legacy_graph_digest, first.legacy_graph_digest);
    assert_ne!(second.graph_digest, first.graph_digest);
    assert_ne!(
        second.projection_digests["claim_sources"],
        first.projection_digests["claim_sources"]
    );

    let generation =
        |store: &Store| graph_generation(&store.connection.lock().unwrap()).unwrap();
    let execute = |store: &Store, sql: &str| {
        store.connection.lock().unwrap().execute(sql, []).unwrap();
    };
    let before = generation(&store);
    execute(
        &store,
        "INSERT INTO mission_runs(id, mission_id, initial_revision, current_generation_id,
                 root_revision, root_run_id, workspace, requester, inputs, mode, status, phase,
                 created_at_unix_ms, updated_at_unix_ms)
             VALUES ('run-example', 'mission/example', 'revision', 'generation', 'revision',
                 'run-example', '/tmp/example', 'person/example', '{}', 'normal', 'running',
                 'normal', '1', '1')",
    );
    assert_eq!(generation(&store), before + 1);
    execute(
        &store,
        "UPDATE mission_runs SET updated_at_unix_ms='2' WHERE id='run-example'",
    );
    assert_eq!(
        generation(&store),
        before + 1,
        "an undigested column must not change the generation"
    );
    execute(
        &store,
        "UPDATE mission_runs SET status='running' WHERE id='run-example'",
    );
    assert_eq!(generation(&store), before + 1);
    execute(
        &store,
        "UPDATE mission_runs SET status='completed' WHERE id='run-example'",
    );
    assert_eq!(generation(&store), before + 2);

    observe_harness(&store, 2);
    let third = store.replication_snapshot().unwrap();
    assert_eq!(GRAPH_DIGESTS_COMPUTED.with(std::cell::Cell::get), 1);
    assert_ne!(third.graph_digest, first.graph_digest);
    assert_eq!(
        third.graph_digest,
        graph_digest(&store.connection.lock().unwrap()).unwrap()
    );
    execute(&store, "DELETE FROM mission_runs WHERE id='run-example'");
    assert_eq!(generation(&store), before + 3);
}

#[test]
fn response_idempotency_caches_never_store_caller_keys() {
    let store = Store::open_memory("node").unwrap();
    let caller_key = "caller-visible-secret-key";
    store
        .cache_idempotency_response(caller_key, &json!({"ok": true}))
        .unwrap();
    assert_eq!(
        store
            .cached_idempotency_response::<Value>(caller_key)
            .unwrap(),
        Some(json!({"ok": true}))
    );
    let connection = store.connection.lock().unwrap();
    let operation_id: String = connection
        .query_row("SELECT operation_id FROM idempotency", [], |row| row.get(0))
        .unwrap();
    assert_ne!(operation_id, caller_key);
    assert_eq!(operation_id, opaque_cache_key(caller_key));
}

#[test]
fn operation_projection_drift_is_detected_and_repaired() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.sqlite3");
    let store = Store::open(&path, "node").unwrap();
    store
        .append_client_claim(&ClaimInput {
            subject: "resource/projection".into(),
            kind: "resource.observed".into(),
            actor: None,
            fields: BTreeMap::from([(
                "kind".into(),
                Value::String("custom.test.projection".into()),
            )]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some("projection-operation".into()),
        })
        .unwrap();
    assert!(store.operation_projection_drift().unwrap().is_empty());
    store
        .connection
        .lock()
        .unwrap()
        .execute("UPDATE operations SET state='conflict'", [])
        .unwrap();
    assert_eq!(store.operation_projection_drift().unwrap().len(), 1);
    drop(store);

    // A start keeps it, so it can answer at once; the repair finds and rebuilds it.
    let reopened = Store::open(&path, "node").unwrap();
    assert_eq!(reopened.operation_projection_drift().unwrap().len(), 1);
    assert!(reopened.repair_operation_projection_drift().unwrap());
    assert!(reopened.operation_projection_drift().unwrap().is_empty());
    assert!(!reopened.repair_operation_projection_drift().unwrap());
}

#[test]
fn legacy_human_gate_mission_keeps_its_revision_during_replication() {
    let store = Store::open_memory("node").unwrap();
    assert!(store.project_replication_backlog().unwrap());
    let intent = parse_intent(
        "version 2\nmission \"sample/review\" state=\"ready\" {\n  goal \"Review a sample change.\"\n  step \"approval\" {\n    gate \"review\" type=\"human\" { reviewer \"person/reviewer\" }\n  }\n}\n",
        "node",
    )
    .unwrap();
    let mut mission = intent.missions["sample/review"].clone();
    mission.revision.clear();
    // This is the serialized form from before human gates had a mode field.
    let current_json = serde_json::to_string(&mission).unwrap();
    let legacy_json = current_json.replace(",\"mode\":\"approve\"", "");
    assert_ne!(legacy_json, current_json);
    let revision = hex::encode(Sha256::digest(legacy_json.as_bytes()));
    let mut body: Value = serde_json::from_str(&legacy_json).unwrap();
    body["revision"] = Value::String(revision.clone());
    let claim = {
        let mut connection = store.connection.lock().unwrap();
        let transaction = connection.transaction().unwrap();
        let claim = append_claim_tx(
            &transaction,
            &store.origin,
            "mission/sample/review",
            "mission.published",
            None,
            &body,
            &[],
            None,
        )
        .unwrap();
        transaction.commit().unwrap();
        claim
    };
    assert!(store.project_replication_backlog().unwrap());
    let connection = store.connection.lock().unwrap();
    let selected: String = connection
        .query_row(
            "SELECT revision FROM mission_revisions WHERE mission_id='sample/review'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(selected, revision);
    assert!(store.operation_projection_drift().unwrap().is_empty());
    assert!(!connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM projection_health WHERE aggregate=?1 AND status='stale')",
            [format!("projection:base:{}", claim.id)],
            |row| row.get::<_, bool>(0),
        )
        .unwrap());
}

#[test]
fn operation_projection_drift_does_not_report_consistent_concurrent_writes() {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(&directory.path().join("state.sqlite3"), "node").unwrap());
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writer = {
        let store = Arc::clone(&store);
        let done = Arc::clone(&done);
        std::thread::spawn(move || {
            for index in 0..64 {
                store
                    .append_client_claim(&ClaimInput {
                        subject: format!("resource/projection-{index}"),
                        kind: "resource.observed".into(),
                        actor: None,
                        fields: BTreeMap::from([(
                            "kind".into(),
                            Value::String("custom.test.projection".into()),
                        )]),
                        evidence: Vec::new(),
                        expected_subject: None,
                        idempotency_key: Some(format!("projection-operation-{index}")),
                    })
                    .unwrap();
            }
            done.store(true, Ordering::Release);
        })
    };
    while !done.load(Ordering::Acquire) {
        assert!(store.operation_projection_drift().unwrap().is_empty());
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    writer.join().unwrap();
    assert!(store.operation_projection_drift().unwrap().is_empty());
}

#[test]
fn work_actions_require_an_active_incarnation_bound_lease() {
    let store = Store::open_memory("node").unwrap();
    let intent = crate::graph::parse_test_intent(
        r#"
version 2

  mission "lease" state="ready" {
    goal "Complete mission lease."
    step "work" { assigned-to "agent/worker" }
    step "other" { assigned-to "agent/worker" }
  }

"#,
        "node",
    )
    .unwrap();
    let planned = store
        .mission(
            &intent,
            IntentInput {
                kdl: "lease".into(),
                source_name: None,
            },
        )
        .unwrap();
    store
        .apply(&intent, &planned.subject_tokens, "lease-mission")
        .unwrap();
    let run = store
        .create_mission_run(&MissionRunRequest {
            mission: "lease".into(),
            revision: None,
            workspace: "/tmp".into(),
            requester: Some("person/test".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "lease-run".into(),
        })
        .unwrap();
    let subject = &run
        .steps
        .iter()
        .find(|step| step.step == "work")
        .unwrap()
        .subject;
    let other = &run
        .steps
        .iter()
        .find(|step| step.step == "other")
        .unwrap()
        .subject;
    store.set_step_state(subject, "ready", None).unwrap();
    store.set_step_state(other, "ready", None).unwrap();
    let queue = store
        .agent_work_queues()
        .unwrap()
        .remove("agent/node.worker")
        .unwrap();
    assert_eq!(queue.active_work_count, 0);
    assert_eq!(queue.queued_work_count, 2);
    assert_eq!(
        queue.next_work_id.as_deref(),
        queue.upcoming_work_ids.first().map(String::as_str)
    );
    assert!(queue.upcoming_work_ids.contains(subject));
    assert!(queue.upcoming_work_ids.contains(other));
    let wake_tag = format!("st3-work:{subject}@1@1@incarnation");
    for (name, recipient) in [
        ("correct-recipient", "agent/node.worker"),
        ("wrong-recipient", "agent/someone.else"),
    ] {
        store
            .append_claim(&ClaimInput {
                subject: format!("message/{name}"),
                kind: "message.sent".into(),
                actor: Some("daemon/runtime".into()),
                fields: BTreeMap::from([
                    ("from".into(), Value::String("daemon/runtime".into())),
                    ("to".into(), Value::String(recipient.into())),
                    ("content".into(), Value::String("wake".into())),
                    ("status".into(), Value::String("sent".into())),
                    (
                        "tags".into(),
                        Value::Array(vec![Value::String(wake_tag.clone())]),
                    ),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some(format!("indexed-wake-{name}")),
            })
            .unwrap();
    }
    let wake = store
        .work_at_snapshot(Some("agent/node.worker"), true, now_ms())
        .unwrap()
        .into_iter()
        .find(|step| step.subject == *subject)
        .unwrap()
        .wake
        .unwrap();
    assert_eq!(
        wake.attempts, 1,
        "a wake for another recipient is not this agent's attempt"
    );
    let mut lean = store
        .work_for_reconcile_all()
        .unwrap()
        .into_iter()
        .find(|step| step.subject == *subject)
        .unwrap();
    assert!(lean.wake.is_none());
    store
        .populate_work_wake_for_reconcile(&mut lean, now_ms())
        .unwrap();
    assert_eq!(lean.wake, Some(wake));
    for (kind, status) in [
        ("message.delivered", "delivered"),
        ("message.read", "read"),
        ("message.closed", "closed"),
    ] {
        store
            .append_claim(&ClaimInput {
                subject: "message/correct-recipient".into(),
                kind: kind.into(),
                actor: Some("agent/node.worker".into()),
                fields: BTreeMap::from([("status".into(), Value::String(status.into()))]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some(format!("{status}-indexed-wake")),
            })
            .unwrap();
    }
    store
        .populate_work_wake_for_reconcile(&mut lean, now_ms())
        .unwrap();
    assert_eq!(
        lean.wake.as_ref().unwrap().acknowledged_by.as_deref(),
        Some("consumed"),
        "a closed wake must not leave an overdue reconcile deadline"
    );
    let request = |incarnation: &str, key: &str| WorkRequest {
        wait_ms: None,
        actor: Some("agent/node.worker".into()),
        incarnation: Some(incarnation.into()),
        summary: None,
        reason: None,
        evidence: Vec::new(),
        idempotency_key: key.into(),
    };

    let missing = WorkRequest {
        wait_ms: None,
        incarnation: None,
        ..request("one", "claim-without-incarnation")
    };
    let error = store.work_action(subject, "claim", &missing).unwrap_err();
    assert_eq!(error.code, "missing-work-incarnation");

    let error = store
        .work_action(
            subject,
            "progress",
            &request("one", "progress-before-claim"),
        )
        .unwrap_err();
    assert_eq!(error.code, "work-not-claimed");
    store
        .work_action(subject, "claim", &request("one", "claim-one"))
        .unwrap();
    let queue = store
        .agent_work_queues()
        .unwrap()
        .remove("agent/node.worker")
        .unwrap();
    assert_eq!(queue.current_work_ids, [subject.clone()]);
    assert_eq!(queue.active_work_count, 1);
    assert_eq!(queue.next_work_id.as_deref(), Some(other.as_str()));
    assert_eq!(queue.queued_work_count, 1);
    let error = store
        .work_action(other, "claim", &request("one", "claim-independent"))
        .unwrap_err();
    assert_eq!(error.code, "agent-capacity");
    let error = store
        .work_action(subject, "progress", &request("two", "progress-two"))
        .unwrap_err();
    assert_eq!(error.code, "wrong-work-incarnation");
    store
        .work_action(subject, "progress", &request("one", "progress-one"))
        .unwrap();
    // Lease expiry changes the read projection before any repair commits.
    store
        .connection
        .lock()
        .unwrap()
        .execute(
            "UPDATE step_runs SET lease_expires_at_unix_ms='0' WHERE subject=?1",
            [subject],
        )
        .unwrap();
    let queue = store
        .agent_work_queues()
        .unwrap()
        .remove("agent/node.worker")
        .unwrap();
    assert!(queue.current_work_ids.is_empty());
    assert_eq!(queue.active_work_count, 0);
    assert_eq!(queue.queued_work_count, 2);
    let seat = store.seat_queue("agent/node.worker").unwrap();
    assert_eq!(queue.current_work_ids, seat.current_work_ids);
    assert_eq!(queue.next_work_id, seat.next_work_id);
    assert_eq!(store.step_run(subject).unwrap().unwrap().status, "ready");
    let reclaimed = store
        .work_action(subject, "claim", &request("two", "reclaim-after-expiry"))
        .unwrap();
    assert_eq!(reclaimed.claim_incarnation.as_deref(), Some("two"));
    assert_eq!(
        store
            .work_action(
                subject,
                "progress",
                &request("one", "old-progress-after-reclaim")
            )
            .unwrap_err()
            .code,
        "wrong-work-incarnation"
    );
    store
        .work_action(
            subject,
            "progress",
            &request("two", "new-progress-after-reclaim"),
        )
        .unwrap();
    store
        .set_step_state(subject, "blocked", Some("waiting for a dependency"))
        .unwrap();

    expire_work_lease_by_claim(&store, subject, "agent/node.worker", "two");
    let reclaimable = store.step_run(subject).unwrap().unwrap();
    assert_eq!(reclaimable.status, "ready");
    assert!(reclaimable.claimant.is_none());
    let error = store
        .work_action(subject, "complete", &request("one", "complete-expired"))
        .unwrap_err();
    assert_eq!(error.code, "work-not-claimed");
    store
        .set_step_state(subject, "ready", Some("the worker lease expired"))
        .unwrap();
    let view = store.step_run(subject).unwrap().unwrap();
    assert!(view.claimant.is_none());
    assert!(view.claim_incarnation.is_none());
    assert!(view.claim_expires_at_unix_ms.is_none());
}

#[test]
fn execution_timing_counts_only_claimed_intervals_and_resets_per_attempt() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("execution-timing.sqlite3");
    let store = Store::open(&path, "source").unwrap();
    let source = r#"
version 2

  mission "execution-timing" state="ready" {
    goal "Measure only active worker execution."
    step "work" timeout="1h" {
      assigned-to "agent/worker"
      retry { attempts 2 }
    }
  }

"#;
    let intent = crate::graph::parse_test_intent(source, "source").unwrap();
    let planned = store
        .mission(
            &intent,
            IntentInput {
                kdl: source.into(),
                source_name: None,
            },
        )
        .unwrap();
    store
        .apply(&intent, &planned.subject_tokens, "execution-timing-mission")
        .unwrap();
    let run = store
        .create_mission_run(&MissionRunRequest {
            mission: "execution-timing".into(),
            revision: None,
            workspace: "/tmp".into(),
            requester: Some("person/test".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "execution-timing-run".into(),
        })
        .unwrap();
    let subject = &run.steps[0].subject;
    store.set_step_state(subject, "ready", None).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(3));
    let waiting = store.step_run(subject).unwrap().unwrap();
    assert_eq!(waiting.execution_started_at_unix_ms, None);
    assert_eq!(waiting.execution_elapsed_ms, 0);
    assert_eq!(waiting.timeout_ms, Some(3_600_000));

    let request = |key: &str| WorkRequest {
        wait_ms: None,
        actor: Some("agent/source.worker".into()),
        incarnation: Some("worker-one".into()),
        summary: None,
        reason: None,
        evidence: Vec::new(),
        idempotency_key: key.into(),
    };
    store
        .work_action(subject, "claim", &request("timing-claim-one"))
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(3));
    let active = store.step_run(subject).unwrap().unwrap();
    assert!(active.execution_started_at_unix_ms.is_some());
    assert!(active.execution_elapsed_ms > 0);
    store
        .work_action(subject, "release", &request("timing-release"))
        .unwrap();
    let released = store.step_run(subject).unwrap().unwrap();
    let first_interval = released.execution_elapsed_ms;
    assert!(first_interval > 0);
    assert_eq!(released.execution_started_at_unix_ms, None);
    std::thread::sleep(std::time::Duration::from_millis(3));
    assert_eq!(
        store
            .step_run(subject)
            .unwrap()
            .unwrap()
            .execution_elapsed_ms,
        first_interval
    );

    store
        .work_action(subject, "claim", &request("timing-claim-two"))
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(3));
    store
        .work_action(subject, "fail", &request("timing-fail"))
        .unwrap();
    let failed = store.step_run(subject).unwrap().unwrap();
    assert!(failed.execution_elapsed_ms > first_interval);
    assert!(store.retry_step(subject, "retry timing", 0).unwrap());
    let retried = store.step_run(subject).unwrap().unwrap();
    assert_eq!(retried.attempt, 2);
    assert_eq!(retried.execution_elapsed_ms, 0);
    assert_eq!(retried.execution_started_at_unix_ms, None);
    drop(store);

    let reopened = Store::open(&path, "source").unwrap();
    assert_eq!(
        reopened
            .step_run(subject)
            .unwrap()
            .unwrap()
            .execution_elapsed_ms,
        0
    );
    let target = Store::open_memory("target").unwrap();
    let exchange = exchange_from(&reopened, &ReplicationInventory::default());
    assert_eq!(receive_and_project(&target, "source", &exchange).invalid, 0);
    let replayed = target.step_run(subject).unwrap().unwrap();
    assert_eq!(replayed.attempt, 2);
    assert_eq!(replayed.execution_elapsed_ms, 0);
}

#[test]
fn legacy_attention_does_not_block_released_work_across_restart_and_replication() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("external-blocker.sqlite3");
    let store = Store::open(&path, "source").unwrap();
    let source = r#"
version 2

mission "external-blocker" state="ready" {
  goal "Wait truthfully for the external host repair."
  step "automated-proof" { assigned-to "agent/ios-owner" }
}
"#;
    let intent = crate::graph::parse_test_intent(source, "source").unwrap();
    let planned = store
        .mission(
            &intent,
            IntentInput {
                kdl: source.into(),
                source_name: None,
            },
        )
        .unwrap();
    store
        .apply(&intent, &planned.subject_tokens, "external-blocker-mission")
        .unwrap();
    let run = store
        .create_mission_run(&MissionRunRequest {
            mission: "external-blocker".into(),
            revision: None,
            workspace: "/tmp".into(),
            requester: Some("person/alex".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "external-blocker-run".into(),
        })
        .unwrap();
    let subject = run.steps[0].subject.clone();
    store.set_step_state(&subject, "ready", None).unwrap();
    let request = |key: &str, reason: Option<&str>| WorkRequest {
        wait_ms: None,
        actor: Some("agent/source.ios-owner".into()),
        incarnation: Some("ios-owner-one".into()),
        summary: None,
        reason: reason.map(str::to_owned),
        evidence: Vec::new(),
        idempotency_key: key.into(),
    };
    store
        .work_action(&subject, "claim", &request("blocker-claim", None))
        .unwrap();
    let attention = store
        .request_attention(
            "attention/example-mac-xcode",
            &AttentionRequest {
                reviewer: "person/alex".into(),
                title: "ExampleMac needs its Xcode simulator components updated".into(),
                reason: "CoreSimulator must be repaired before automated proof can run.".into(),
                severity: "error".into(),
                targets: vec!["host/example-mac".into(), subject.clone()],
                actor: "agent/source.ios-owner".into(),
                idempotency_key: "example-mac-xcode-attention".into(),
            },
        )
        .unwrap();
    let reason = "ExampleMac has an exact CoreSimulator/CoreDevice mismatch; renewing this claim would be idle and misleading.";
    store
        .work_action(
            &subject,
            "release",
            &request("blocker-release", Some(reason)),
        )
        .unwrap();

    let ready = store.step_run(&subject).unwrap().unwrap();
    assert_eq!(ready.status, "ready");
    assert!(ready.blockers.is_empty());
    drop(store);

    let store = Store::open(&path, "source").unwrap();
    assert_eq!(store.step_run(&subject).unwrap().unwrap().status, "ready");
    let replica = Store::open_memory("replica").unwrap();
    let exchange = exchange_from(&store, &ReplicationInventory::default());
    assert_eq!(
        receive_and_project(&replica, "source", &exchange).invalid,
        0
    );
    let replicated = replica.step_run(&subject).unwrap().unwrap();
    assert_eq!(replicated.status, "ready");
    assert!(replicated.blockers.is_empty());

    store
        .resolve_attention(
            &attention.subject,
            &AttentionResolveRequest {
                delegation: None,
                outcome: "resolved".into(),
                reason: Some("Xcode first-launch setup now succeeds.".into()),
                actor: "person/alex".into(),
                idempotency_key: "resolve-example-mac-xcode".into(),
            },
        )
        .unwrap();
    let reopened = store.step_run(&subject).unwrap().unwrap();
    assert_eq!(reopened.status, "ready");
    assert_eq!(reopened.blocked_reason, None);
    assert!(reopened.blockers.is_empty());
    store
        .work_action(&subject, "claim", &request("unblocked-reclaim", None))
        .unwrap();
}

#[test]
fn terminal_cascade_preserves_completed_children_across_three_members() {
    use smallclaims::store::set_thread_clock;
    struct ResetClock;
    impl Drop for ResetClock {
        fn drop(&mut self) {
            set_thread_clock(None);
        }
    }
    let _reset = ResetClock;
    let source = Store::open_memory("alder").unwrap();
    let birch = Store::open_memory("birch").unwrap();
    let cedar = Store::open_memory("cedar").unwrap();
    let members = [&source, &birch, &cedar];
    let start = now_ms() + 60_000;
    let at = |time| {
        set_thread_clock(Some(time));
        source.set_write_clock_at(time).unwrap();
    };
    at(start);
    publish_mission(
        &source,
        r#"version 2
    mission "terminal-stamps-root" state="ready" {
      goal "Keep each completed child's own completion time."
      step "work" { agentless }
    }
    mission "terminal-stamps-child" state="ready" {
      goal "Complete one child before admitting the next."
      step "work" { agentless }
    }"#,
        "terminal-stamps-mission",
    );
    let request = |mission: &str, key: &str| MissionRunRequest {
        mission: mission.into(),
        revision: None,
        workspace: "/tmp".into(),
        requester: Some("person/test".into()),
        mode: Some("run".into()),
        inputs: BTreeMap::new(),
        idempotency_key: key.into(),
    };
    let root = source
        .create_mission_run(&request("terminal-stamps-root", "root"))
        .unwrap();
    let exchange_all = || {
        for sender in members {
            for receiver in members {
                if sender.origin != receiver.origin {
                    let exchange =
                        exchange_from(sender, &receiver.replication_inventory().unwrap());
                    assert_eq!(
                        receive_and_project(receiver, &sender.origin, &exchange).invalid,
                        0
                    );
                }
            }
        }
    };
    // Retain the actual immutable lifecycle inputs, not a synthesized receipt or wall-clock
    // estimate. Sorting by id compares bytes independently of local arrival/store positions.
    let lifecycle_inputs = |member: &Store| {
        let connection = member.readers.get();
        let mut statement = connection
            .prepare(
                "SELECT id, subject, kind, accepted_at_unix_ms, body, predecessors
                     FROM claims WHERE kind IN ('mission-run.state','run-generation.state')
                     ORDER BY id",
            )
            .unwrap();
        statement
            .query_map([], |row| {
                Ok([
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                ])
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    };
    exchange_all();
    // Both declarations retain the default one-active-run limit. Complete and project
    // each child before admitting the next child; the distinct root remains active.
    let children = ["child-one", "child-two"].map(|key| {
        let index = if key == "child-one" { 0 } else { 1 };
        at(start + index * 1_000 + 500);
        let child = source
            .create_child_mission_run(
                &request("terminal-stamps-child", key),
                &root,
                &root.steps[0].subject,
                None,
            )
            .unwrap();
        exchange_all();
        at(start + (index + 1) * 1_000);
        assert!(
            source
                .set_mission_run_state(&child.id, "completed", "terminal", None)
                .unwrap()
        );
        assert_eq!(
            source.mission_run_status(&child.id).unwrap().as_deref(),
            Some("completed"),
            "the child must release its mission capacity before the next admission"
        );
        assert_eq!(
            source.mission_run_status(&root.id).unwrap().as_deref(),
            Some("running"),
            "the root stays live until its separate later terminal claim"
        );
        exchange_all();
        child
    });
    let child_inputs = lifecycle_inputs(&source);
    assert_eq!(
        child_inputs.len(),
        4,
        "two actual terminal claims per child"
    );
    at(start + 3_000);
    assert!(
        source
            .set_mission_run_state(&root.id, "completed", "terminal", None)
            .unwrap()
    );
    exchange_all();
    let expected_inputs = lifecycle_inputs(&source);
    assert_eq!(expected_inputs.len(), 6, "root plus two completed children");
    assert_eq!(
        expected_inputs
            .iter()
            .filter(|input| child_inputs.iter().any(|child| child[0] == input[0]))
            .cloned()
            .collect::<Vec<_>>(),
        child_inputs,
        "a later parent claim never replaces the children's immutable inputs"
    );
    let runs = [&children[0], &children[1], &root];
    let expected = runs.map(|run| {
        let generation: String = source
            .readers
            .get()
            .query_row(
                "SELECT current_generation_id FROM mission_runs WHERE id=?1",
                [&run.id],
                |row| row.get(0),
            )
            .unwrap();
        let accepted = |subject: String, kind: &str| {
            let claims = expected_inputs
                .iter()
                .filter(|input| input[1] == subject && input[2] == kind)
                .collect::<Vec<_>>();
            assert_eq!(claims.len(), 1, "one real terminal state for {subject}");
            let body: Value = serde_json::from_str(&claims[0][4]).unwrap();
            assert_eq!(body["fields"]["status"], "completed");
            assert_eq!(body["fields"]["phase"], "terminal");
            claims[0][3].clone()
        };
        let run_time = accepted(format!("mission-run/{}", run.id), "mission-run.state");
        let generation_time = accepted(
            format!("run-generation/{generation}"),
            "run-generation.state",
        );
        (generation, run_time, generation_time)
    });
    for child in &expected[..2] {
        assert_ne!(
            child.1, expected[2].1,
            "distinct child/root run times discriminate retiming"
        );
        assert_ne!(
            child.2, expected[2].2,
            "distinct child/root generation times discriminate retiming"
        );
    }
    let inventory = source.replication_inventory().unwrap();
    let mut captures = Vec::new();
    // Check every member's actual inputs and terminal state BEFORE any date/digest assertion.
    for member in members {
        assert_eq!(
            member.replication_inventory().unwrap().digest,
            inventory.digest,
            "same envelope cut: {}",
            member.origin
        );
        assert_eq!(
            lifecycle_inputs(member),
            expected_inputs,
            "same lifecycle bytes: {}",
            member.origin
        );
        let connection = member.readers.get();
        let mut states = Vec::new();
        for (index, run) in runs.iter().enumerate() {
            let state = connection
                .query_row(
                    "SELECT g.id, r.status, r.phase, r.updated_at_unix_ms,
                            g.status, g.updated_at_unix_ms
                     FROM mission_runs r JOIN run_generations g ON g.id=r.current_generation_id
                     WHERE r.id=?1",
                    [&run.id],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, String>(4)?,
                            row.get::<_, String>(5)?,
                        ))
                    },
                )
                .unwrap();
            assert_eq!(
                state.0, expected[index].0,
                "exact generation: {} {}",
                member.origin, run.id
            );
            assert_eq!(
                (state.1.as_str(), state.2.as_str(), state.4.as_str()),
                ("completed", "terminal", "completed"),
                "status before date: {} {}",
                member.origin,
                run.id
            );
            states.push(state);
        }
        captures.push((member.origin.clone(), states));
    }
    for (member, states) in captures {
        for (index, state) in states.iter().enumerate() {
            assert_eq!(
                (&state.3, &state.5),
                (&expected[index].1, &expected[index].2),
                "immutable completion dates: {member} {}",
                runs[index].id
            );
        }
    }
    // The tiny-fixture full-row oracle is independent of the maintained digest cache.
    let expected_digests = projection_digest::oracle(&source.readers.get()).unwrap();
    for member in members {
        let reader = member.readers.get();
        let cached = projection_digest::tables(&reader).unwrap();
        let oracle = projection_digest::oracle(&reader).unwrap();
        for table in ["mission_runs", "run_generations"] {
            assert_eq!(
                cached[table], oracle[table],
                "{}: {table} cache/rows",
                member.origin
            );
            assert_eq!(
                oracle[table], expected_digests[table],
                "{}: {table} three-member parity",
                member.origin
            );
        }
    }
}

#[test]
fn terminal_owners_retire_every_work_state_across_restart_and_replication() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("terminal-owner.sqlite3");
    let store = Store::open(&path, "source").unwrap();
    let source = r#"
version 2

  mission "terminal-owner" state="ready" {
    goal "Make terminal ownership authoritative."
    step "active" { assigned-to "agent/worker" }
    step "expired" { assigned-to "agent/worker" }
    step "ready" { assigned-to "agent/worker" }
    step "blocked" { assigned-to "agent/worker" }
    step "pending" { assigned-to "agent/worker" }
  }

"#;
    let intent = crate::graph::parse_test_intent(source, "source").unwrap();
    let planned = store
        .mission(
            &intent,
            IntentInput {
                kdl: source.into(),
                source_name: None,
            },
        )
        .unwrap();
    store
        .apply(&intent, &planned.subject_tokens, "terminal-owner-mission")
        .unwrap();
    let run = store
        .create_mission_run(&MissionRunRequest {
            mission: "terminal-owner".into(),
            revision: None,
            workspace: "/tmp".into(),
            requester: Some("person/test".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "terminal-owner-run".into(),
        })
        .unwrap();
    let subjects = run
        .steps
        .iter()
        .map(|step| (step.step.clone(), step.subject.clone()))
        .collect::<BTreeMap<_, _>>();
    let request = |key: &str| WorkRequest {
        wait_ms: None,
        actor: Some("agent/source.worker".into()),
        incarnation: Some("worker-one".into()),
        summary: None,
        reason: None,
        evidence: Vec::new(),
        idempotency_key: key.into(),
    };
    for path in ["active", "expired", "ready"] {
        store
            .set_step_state(&subjects[path], "ready", None)
            .unwrap();
    }
    store
        .set_step_state(&subjects["blocked"], "blocked", Some("waiting for a gate"))
        .unwrap();
    store
        .work_action(&subjects["expired"], "claim", &request("claim-expired"))
        .unwrap();
    expire_work_lease_by_claim(
        &store,
        &subjects["expired"],
        "agent/source.worker",
        "worker-one",
    );
    let expired = store.step_run(&subjects["expired"]).unwrap().unwrap();
    assert_eq!(expired.status, "ready");
    assert!(expired.claimant.is_none());
    store
        .work_action(&subjects["active"], "claim", &request("claim-active"))
        .unwrap();

    assert!(
        store
            .set_mission_run_state(&run.id, "cancelled", "terminal", Some("the owner stopped"),)
            .unwrap()
    );
    let terminal = store.mission_run(&run.id).unwrap().unwrap();
    assert!(terminal.steps.iter().all(|step| step.status == "cancelled"));
    assert!(terminal.steps.iter().all(|step| {
        step.claimant.is_none()
            && step.claim_incarnation.is_none()
            && step.claim_expires_at_unix_ms.is_none()
    }));
    assert!(store.work(None, false).unwrap().is_empty());
    assert_eq!(store.work(None, true).unwrap().len(), 5);
    for subject in subjects.values() {
        let claim = store
            .latest_claim(subject, Some("step-run.state"))
            .unwrap()
            .expect("terminal descendant claim");
        assert_eq!(
            claim.body.pointer("/fields/status").and_then(Value::as_str),
            Some("cancelled")
        );
    }

    let error = store
        .work_action(
            &subjects["active"],
            "claim",
            &request("claim-after-terminal"),
        )
        .unwrap_err();
    assert_eq!(error.code, "terminal-work-owner");
    {
        let mut connection = store.connection.lock().unwrap();
        let transaction = connection.transaction().unwrap();
        let stale_owner = append_claim_tx(
            &transaction,
            &store.origin,
            &run.subject,
            "mission-run.state",
            None,
            &json!({"fields": {
                "status": "running",
                "phase": "normal",
                "reason": "a stale replica tried to reopen the owner",
            }}),
            &[],
            None,
        )
        .unwrap();
        project_mission_run_update(&transaction, &stale_owner).unwrap();
        let stale_work = append_claim_tx(
            &transaction,
            &store.origin,
            &subjects["active"],
            "work.renewed",
            Some("agent/source.worker"),
            &json!({"fields": {
                "attempt": 1,
                "status": "working",
                "worker_reported": false,
                "claimant": "agent/source.worker",
                "claim_incarnation": "stale-worker",
                "claim_expires_at_unix_ms": now_ms() + 600_000,
                "readiness_epoch": 1,
            }}),
            &[],
            None,
        )
        .unwrap();
        project_mission_run_update(&transaction, &stale_work).unwrap();
        transaction.commit().unwrap();
    }
    let still_terminal = store.mission_run(&run.id).unwrap().unwrap();
    assert_eq!(still_terminal.status, "cancelled");
    assert_eq!(
        still_terminal
            .steps
            .iter()
            .find(|step| step.step == "active")
            .unwrap()
            .status,
        "cancelled"
    );
    drop(store);

    let reopened = Store::open(&path, "source").unwrap();
    let restarted = reopened.mission_run(&run.id).unwrap().unwrap();
    assert!(
        restarted
            .steps
            .iter()
            .all(|step| step.status == "cancelled")
    );
    assert!(reopened.work(None, false).unwrap().is_empty());

    let target = Store::open_memory("target").unwrap();
    let exchange = exchange_from(&reopened, &ReplicationInventory::default());
    let admission = receive_and_project(&target, "source", &exchange);
    assert_eq!(admission.invalid, 0);
    let replayed = target.mission_run(&run.id).unwrap().unwrap();
    assert_eq!(replayed.status, "cancelled");
    assert!(replayed.steps.iter().all(|step| step.status == "cancelled"));
    assert!(replayed.steps.iter().all(|step| step.claimant.is_none()));
    assert!(target.work(None, false).unwrap().is_empty());
}

#[test]
fn terminal_roots_reap_nested_orphans_idempotently_across_restart_and_replication() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("terminal-tree.sqlite3");
    let store = Store::open(&path, "source").unwrap();
    let publish = |source: &str, key: &str| {
        let intent = crate::graph::parse_test_intent(source, "source").unwrap();
        let planned = store
            .mission(
                &intent,
                IntentInput {
                    kdl: source.into(),
                    source_name: None,
                },
            )
            .unwrap();
        store.apply(&intent, &planned.subject_tokens, key).unwrap();
    };
    publish(
        r#"version 2
 mission "tree-root" state="ready" {
   goal "Own nested work."
   step "child" { agentless }
 }"#,
        "terminal-tree-root",
    );
    publish(
        r#"version 2
 mission "tree-child" state="ready" {
   goal "Expose nested work."
   step "work" { assigned-to "agent/worker" }
 }"#,
        "terminal-tree-child",
    );
    let root = store
        .create_mission_run(&MissionRunRequest {
            mission: "tree-root".into(),
            revision: None,
            workspace: "/tmp".into(),
            requester: Some("person/test".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "terminal-tree-root-run".into(),
        })
        .unwrap();
    let child = store
        .create_child_mission_run(
            &MissionRunRequest {
                mission: "tree-child".into(),
                revision: None,
                workspace: "/tmp".into(),
                requester: Some("person/test".into()),
                mode: Some("run".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "terminal-tree-child-run".into(),
            },
            &root,
            &root.steps[0].subject,
            None,
        )
        .unwrap();
    let child_step = child.steps[0].subject.clone();
    store.set_step_state(&child_step, "ready", None).unwrap();

    assert!(
        store
            .set_mission_run_state(&root.id, "failed", "terminal", Some("the root failed"),)
            .unwrap()
    );
    let reaped = store.mission_run(&child.id).unwrap().unwrap();
    assert_eq!(
        (reaped.status.as_str(), reaped.phase.as_str()),
        ("cancelled", "terminal")
    );
    assert_eq!(reaped.steps[0].status, "cancelled");
    assert!(store.work(None, false).unwrap().is_empty());

    // Reproduce the historical orphan projection, then prove that the same terminal
    // transition repairs it and a duplicate repair is an explicit no-op.
    store
        .connection
        .lock()
        .unwrap()
        .execute_batch(&format!(
            "UPDATE mission_runs SET status='running', phase='normal' WHERE id='{}';
                 UPDATE run_generations SET status='running' WHERE id='{}';
                 UPDATE step_runs SET status='ready', blocked_reason=NULL WHERE subject='{}';",
            child.id,
            generation_id_from_subject(&child.generation),
            child_step,
        ))
        .unwrap();
    // This repair touches run-state projections and appends new history, not any
    // captured body, canonical key or protection reference.
    store.checkpoint_sealed_set(now_ms() + 1_000).unwrap();
    let before_dry_run = store.index().unwrap();
    let capture_epoch =
        smallclaims::store::checkpoint_capture_epoch(&store.readers.get()).unwrap();
    let repair = store.operational_repair_plan().unwrap();
    assert_eq!(store.index().unwrap(), before_dry_run);
    assert_eq!(
        smallclaims::store::checkpoint_capture_epoch(&store.readers.get()).unwrap(),
        capture_epoch,
        "planning a repair must not invalidate checkpoint capture"
    );
    assert_eq!(repair.status, "changes");
    assert!(repair.items.iter().any(|item| {
        item.class == "terminal-descendants"
            && item.subject == root.subject
            && item.affected_subjects.contains(&child.subject)
            && item.affected_subjects.contains(&child_step)
    }));
    let applied = store.apply_operational_repair(&repair.token).unwrap();
    assert!(applied.applied >= 1);
    assert!(!applied.already_applied);
    let repaired_epoch =
        smallclaims::store::checkpoint_capture_epoch(&store.readers.get()).unwrap();
    assert_eq!(
        repaired_epoch,
        capture_epoch,
        "unrelated operational-state repair must not invalidate captured history"
    );
    let duplicate = store.apply_operational_repair(&repair.token).unwrap();
    assert_eq!(duplicate.applied, 0);
    assert!(duplicate.already_applied);
    assert_eq!(
        smallclaims::store::checkpoint_capture_epoch(&store.readers.get()).unwrap(),
        repaired_epoch,
        "an already-applied repair must not invalidate checkpoint capture"
    );
    assert_eq!(store.operational_repair_plan().unwrap().status, "clean");
    let history = store.work(None, true).unwrap();
    let nested = history
        .iter()
        .find(|step| step.subject == child_step)
        .unwrap();
    assert_eq!(nested.status, "cancelled");
    assert_eq!(nested.run, child.subject);
    assert!(
        store
            .work_annotation(nested)
            .unwrap()
            .reasons
            .contains(&"terminal-root-owner".to_owned())
    );
    let batched = store.work_annotations(&history).unwrap();
    assert_eq!(
        serde_json::to_value(store.work_annotation(nested).unwrap()).unwrap(),
        serde_json::to_value(&batched[&nested.subject]).unwrap()
    );

    let fresh = store
        .create_mission_run(&MissionRunRequest {
            mission: "tree-child".into(),
            revision: None,
            workspace: "/tmp".into(),
            requester: Some("person/test".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "terminal-tree-fresh-run".into(),
        })
        .unwrap();
    store
        .set_step_state(&fresh.steps[0].subject, "ready", None)
        .unwrap();
    assert_eq!(store.work(None, false).unwrap().len(), 1);
    drop(store);

    let reopened = Store::open(&path, "source").unwrap();
    let restarted = reopened.mission_run(&child.id).unwrap().unwrap();
    assert_eq!(
        (restarted.status.as_str(), restarted.phase.as_str()),
        ("cancelled", "terminal")
    );
    assert_eq!(restarted.steps[0].status, "cancelled");

    let target = Store::open_memory("target").unwrap();
    let exchange = exchange_from(&reopened, &ReplicationInventory::default());
    let admission = receive_and_project(&target, "source", &exchange);
    assert_eq!(admission.invalid, 0);
    let replayed = target.mission_run(&child.id).unwrap().unwrap();
    assert_eq!(
        (replayed.status.as_str(), replayed.phase.as_str()),
        ("cancelled", "terminal")
    );
    assert_eq!(replayed.steps[0].status, "cancelled");
    assert_eq!(target.operational_repair_plan().unwrap().status, "clean");
    let replicated_retry = target.apply_operational_repair(&repair.token).unwrap();
    assert_eq!(replicated_retry.applied, 0);
    assert!(replicated_retry.already_applied);
}

#[test]
fn operational_repair_closes_expired_claims_and_wake_contradictions() {
    let store = Store::open_memory("node").unwrap();
    let source = r#"version 2
 agent "worker" { workspace "/tmp"; command "true" }
 mission "repairable" state="ready" {
   goal "Exercise bounded operational repair."
   step "work" { assigned-to "agent/node.worker" }
 }"#;
    let intent = crate::graph::parse_test_intent(source, "node").unwrap();
    let planned = store
        .mission(
            &intent,
            IntentInput {
                kdl: source.into(),
                source_name: None,
            },
        )
        .unwrap();
    store
        .apply(&intent, &planned.subject_tokens, "repairable-mission")
        .unwrap();
    let run = store
        .create_mission_run(&MissionRunRequest {
            mission: "repairable".into(),
            revision: None,
            workspace: "/tmp".into(),
            requester: Some("person/operator".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "repairable-run".into(),
        })
        .unwrap();
    let step = run.steps[0].subject.clone();
    store.set_step_state(&step, "ready", None).unwrap();
    store
        .work_action(
            &step,
            "claim",
            &WorkRequest {
                wait_ms: None,
                actor: Some("agent/node.worker".into()),
                incarnation: Some("worker-one".into()),
                summary: None,
                reason: None,
                evidence: Vec::new(),
                idempotency_key: "repair-expired-claim".into(),
            },
        )
        .unwrap();
    expire_work_lease_by_claim(&store, &step, "agent/node.worker", "worker-one");

    store
        .request_attention(
            "attention/stale-repair",
            &AttentionRequest {
                reviewer: "person/operator".into(),
                title: "Stale repair attention".into(),
                reason: "a superseded target still appears pending".into(),
                severity: "warning".into(),
                targets: vec!["step-run/missing-generation/missing-step".into()],
                actor: "person/operator".into(),
                idempotency_key: "stale-repair-attention".into(),
            },
        )
        .unwrap();
    // The Typecase shape: the mission it names was cancelled after the request.
    let typecase = start_agentless_run(&store, "typecase");
    request_fault(&store, "attention/typecase", &["mission/typecase"]);
    store
        .set_mission_run_state(
            &typecase.id,
            "cancelled",
            "terminal",
            Some("moved to a seat"),
        )
        .unwrap();

    store
        .append_claim(&ClaimInput {
            subject: "agent/node.worker".into(),
            kind: "runtime.observed".into(),
            actor: None,
            fields: BTreeMap::from([
                ("status".into(), Value::String("running".into())),
                ("runtime_id".into(), Value::String("node.worker".into())),
                ("terminal".into(), Value::Bool(true)),
                ("incarnation_id".into(), Value::String("worker-one".into())),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some("repair-runtime".into()),
        })
        .unwrap();
    let diagnostic = store
        .append_claim(&ClaimInput {
            subject: "agent/node.worker".into(),
            kind: "harness.diagnostic".into(),
            actor: Some("agent/node.worker".into()),
            fields: BTreeMap::from([
                ("severity".into(), Value::String("error".into())),
                ("status".into(), Value::String("failed".into())),
                ("code".into(), Value::String("work-wake-exhausted".into())),
                (
                    "reason".into(),
                    Value::String("the legacy wake was not acknowledged".into()),
                ),
                ("incarnation_id".into(), Value::String("worker-one".into())),
                ("step_run".into(), Value::String(step.clone())),
                ("wake_attempts".into(), Value::from(3)),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some("repair-wake-diagnostic".into()),
        })
        .unwrap();
    store
        .append_claim(&ClaimInput {
            subject: "agent/node.worker".into(),
            kind: "harness.observed".into(),
            actor: Some("agent/node.worker".into()),
            fields: BTreeMap::from([
                ("state".into(), Value::String("working".into())),
                ("driver".into(), Value::String("codex".into())),
                ("incarnation_id".into(), Value::String("worker-one".into())),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some("repair-wake-ack".into()),
        })
        .unwrap();

    let plan = store.operational_repair_plan().unwrap();
    let classes = plan
        .items
        .iter()
        .map(|item| item.class.as_str())
        .collect::<BTreeSet<_>>();
    assert!(classes.contains("expired-claim"));
    assert!(classes.contains("wake-contradiction"));
    assert!(!classes.contains("superseded-attention"));
    let wake = plan
        .items
        .iter()
        .find(|item| item.class == "wake-contradiction")
        .unwrap();
    assert_eq!(
        wake.details.get("diagnostic_claim").and_then(Value::as_str),
        Some(diagnostic.id.as_str())
    );
    assert!(
        wake.affected_subjects
            .contains(&"agent/node.worker".to_owned())
    );
    assert!(wake.affected_subjects.contains(&step));

    let stale = store
        .apply_operational_repair(&format!("orpv0:{}", "0".repeat(64)))
        .unwrap_err();
    assert_eq!(stale.code, "stale-repair-plan");

    let result = store.apply_operational_repair(&plan.token).unwrap();
    assert_eq!(result.applied, plan.items.len());
    assert_eq!(store.step_run(&step).unwrap().unwrap().status, "ready");
    assert!(
        store
            .attention_items(Some("person/operator"))
            .unwrap()
            .is_empty()
    );
    let recovered = store
        .latest_claim("agent/node.worker", Some("harness.diagnostic"))
        .unwrap()
        .unwrap();
    assert_eq!(
        recovered
            .body
            .pointer("/fields/status")
            .and_then(Value::as_str),
        Some("recovered")
    );
    assert_eq!(store.operational_repair_plan().unwrap().status, "clean");
    let duplicate = store.apply_operational_repair(&plan.token).unwrap();
    assert_eq!(duplicate.applied, 0);
    assert!(duplicate.already_applied);
}

#[test]
fn operational_repair_terminalizes_a_stale_cancelled_final_phase() {
    let store = Store::open_memory("node").unwrap();
    let source = r#"version 2
mission "cancelled-final-repair" state="ready" {
  goal "Prove stale cancellation repair."
  step "work" { agentless }
  finally { step "cleanup" { agentless } }
}
"#;
    let intent = crate::graph::parse_test_intent(source, "node").unwrap();
    let planned = store
        .mission(
            &intent,
            IntentInput {
                kdl: source.into(),
                source_name: None,
            },
        )
        .unwrap();
    store
        .apply(
            &intent,
            &planned.subject_tokens,
            "cancelled-final-repair-source",
        )
        .unwrap();
    let run = store
        .create_mission_run(&MissionRunRequest {
            mission: "cancelled-final-repair".into(),
            revision: None,
            workspace: "/tmp".into(),
            requester: Some("person/test".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "cancelled-final-repair-run".into(),
        })
        .unwrap();
    store
        .request_mission_run_cancellation(&run.id, "the operator cancelled")
        .unwrap();
    store
        .connection
        .lock()
        .unwrap()
        .execute(
            "UPDATE mission_runs SET updated_at_unix_ms='0' WHERE id=?1",
            [&run.id],
        )
        .unwrap();

    let plan = store.operational_repair_plan().unwrap();
    let item = plan
        .items
        .iter()
        .find(|item| item.class == "cancelled-final-stall")
        .expect("the stale cancelled final phase needs a bounded repair");
    assert_eq!(item.subject, run.subject);
    assert!(
        item.affected_subjects
            .iter()
            .any(|subject| subject.ends_with("/cleanup"))
    );

    let result = store.apply_operational_repair(&plan.token).unwrap();
    assert!(result.applied >= 1);
    let repaired = store.mission_run(&run.id).unwrap().unwrap();
    assert_eq!(
        (repaired.status.as_str(), repaired.phase.as_str()),
        ("cancelled", "terminal")
    );
    assert!(
        repaired
            .steps
            .iter()
            .all(|step| matches!(step.status.as_str(), "completed" | "failed" | "cancelled"))
    );
    assert_eq!(store.operational_repair_plan().unwrap().status, "clean");
}

#[test]
fn operational_repair_activates_a_stale_safe_mission_root() {
    let store = Store::open_memory("node").unwrap();
    let source = r#"version 2
mission "dispatch-repair" state="ready" {
  goal "Recover a missed readiness pass."
  step "root" { agentless }
  step "later" { agentless; depends-on { step "root" completed } }
}
"#;
    let intent = crate::graph::parse_test_intent(source, "node").unwrap();
    let planned = store
        .mission(
            &intent,
            IntentInput {
                kdl: source.into(),
                source_name: None,
            },
        )
        .unwrap();
    store
        .apply(&intent, &planned.subject_tokens, "dispatch-repair-source")
        .unwrap();
    let run = store
        .create_mission_run(&MissionRunRequest {
            mission: "dispatch-repair".into(),
            revision: None,
            workspace: "/tmp".into(),
            requester: Some("person/test".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "dispatch-repair-run".into(),
        })
        .unwrap();
    store
        .connection
        .lock()
        .unwrap()
        .execute(
            "UPDATE mission_runs SET updated_at_unix_ms='0' WHERE id=?1",
            [&run.id],
        )
        .unwrap();

    let plan = store.operational_repair_plan().unwrap();
    let item = plan
        .items
        .iter()
        .find(|item| item.class == "mission-dispatch-stall")
        .expect("the missed initial readiness pass needs a bounded repair");
    assert_eq!(item.subject, run.subject);
    assert_eq!(
        item.details["step_runs"],
        json!([format!(
            "step-run/{}/root",
            generation_id_from_subject(&run.generation)
        )])
    );

    let result = store.apply_operational_repair(&plan.token).unwrap();
    assert!(result.applied >= 1);
    let repaired = store.mission_run(&run.id).unwrap().unwrap();
    assert_eq!(
        repaired
            .steps
            .iter()
            .find(|step| step.step == "root")
            .unwrap()
            .status,
        "ready"
    );
    assert_eq!(
        repaired
            .steps
            .iter()
            .find(|step| step.step == "later")
            .unwrap()
            .status,
        "pending"
    );
    assert_eq!(store.operational_repair_plan().unwrap().status, "clean");
}

#[test]
fn an_active_nested_lease_can_publish_its_parent_mission_output() {
    let store = Store::open_memory("node").unwrap();
    let publish = |source: &str, key: &str| {
        let intent = crate::graph::parse_test_intent(source, "node").unwrap();
        let planned = store
            .mission(
                &intent,
                IntentInput {
                    kdl: source.into(),
                    source_name: None,
                },
            )
            .unwrap();
        store.apply(&intent, &planned.subject_tokens, key).unwrap();
    };
    publish(
        r#"version 2

  mission "bootstrap" state="ready" {
    goal "Complete mission bootstrap."
    step "compile" {
      assigned-to "agent/planner"
      produces-mission "project/work"
      mission "work" { goal "Complete mission work."; step "publish" { } }
    }
  }
"#,
        "nested-output-bootstrap",
    );
    publish(
        r#"version 2
 mission "project/work" state="ready" { goal "Run project work."; step "work" { } } "#,
        "nested-output-mission",
    );
    let mission = store.mission_spec("project/work", None).unwrap().unwrap();
    let run = store
        .create_mission_run(&MissionRunRequest {
            mission: "bootstrap".into(),
            revision: None,
            workspace: "/tmp".into(),
            requester: Some("person/test".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "nested-output-run".into(),
        })
        .unwrap();
    let parent = run
        .steps
        .iter()
        .find(|step| step.step == "compile")
        .unwrap();
    let child = run
        .steps
        .iter()
        .find(|step| step.step == "compile/work/publish")
        .unwrap();
    let request = |key: &str| WorkRequest {
        wait_ms: None,
        actor: Some("agent/node.planner".into()),
        incarnation: Some("current".into()),
        summary: None,
        reason: None,
        evidence: Vec::new(),
        idempotency_key: key.into(),
    };
    store
        .set_step_state(&parent.subject, "ready", None)
        .unwrap();
    store
        .work_action(&parent.subject, "claim", &request("claim-parent"))
        .unwrap();
    store
        .work_action(&parent.subject, "complete", &request("complete-parent"))
        .unwrap();
    store.set_step_state(&child.subject, "ready", None).unwrap();
    store
        .work_action(&child.subject, "claim", &request("claim-child"))
        .unwrap();

    assert!(
        store
            .mission_output_authorized(&parent.subject, "agent/node.planner", Some("current"))
            .unwrap()
    );
    assert!(
        !store
            .mission_output_authorized(&parent.subject, "agent/node.planner", Some("stale"))
            .unwrap()
    );
    let output = store
        .record_mission_output(
            &parent.subject,
            "agent/node.planner",
            Some("current"),
            "project/work",
            &mission,
            "bind-nested-output",
        )
        .unwrap();
    assert_eq!(output.revision, mission.revision);
}

#[test]
fn nested_work_renews_the_same_workers_ancestor_lease() {
    let store = Store::open_memory("node").unwrap();
    let source = r#"version 2

agent "worker" { workspace "/tmp"; command "true" }

mission "nested-work" state="ready" {
  goal "Complete nested work."
  step "outer" {
    assigned-to "agent/worker"
    mission "work" {
      goal "Complete the child work."
      step "first" { }
      step "second" { depends-on { step "first" completed } }
    }

  }
}
"#;
    let intent = crate::graph::parse_test_intent(source, "node").unwrap();
    let planned = store
        .mission(
            &intent,
            IntentInput {
                kdl: source.into(),
                source_name: None,
            },
        )
        .unwrap();
    store
        .apply(&intent, &planned.subject_tokens, "nested-lease-source")
        .unwrap();
    let run = store
        .create_mission_run(&MissionRunRequest {
            mission: "nested-work".into(),
            revision: None,
            workspace: "/tmp".into(),
            requester: Some("person/test".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "nested-lease-run".into(),
        })
        .unwrap();
    let parent = run.steps.iter().find(|step| step.step == "outer").unwrap();
    let child = run
        .steps
        .iter()
        .find(|step| step.step == "outer/work/first")
        .unwrap();
    let request = |key: &str| WorkRequest {
        wait_ms: None,
        actor: Some("agent/node.worker".into()),
        incarnation: Some("current".into()),
        summary: None,
        reason: None,
        evidence: Vec::new(),
        idempotency_key: key.into(),
    };
    store
        .set_step_state(&parent.subject, "ready", None)
        .unwrap();
    store
        .work_action(&parent.subject, "claim", &request("claim-parent"))
        .unwrap();
    store
        .work_action(&parent.subject, "progress", &request("progress-parent"))
        .unwrap();
    store.set_step_state(&child.subject, "ready", None).unwrap();
    store
        .work_action(&child.subject, "claim", &request("claim-child"))
        .unwrap();
    store
        .connection
        .lock()
        .unwrap()
        .execute(
            "UPDATE step_runs SET lease_expires_at_unix_ms='0' WHERE subject=?1",
            [&parent.subject],
        )
        .unwrap();

    store
        .work_action(&child.subject, "complete", &request("complete-child"))
        .unwrap();

    let parent = store.step_run(&parent.subject).unwrap().unwrap();
    assert_eq!(parent.status, "working");
    assert_eq!(parent.claimant.as_deref(), Some("agent/node.worker"));
    assert_eq!(parent.claim_incarnation.as_deref(), Some("current"));
    assert!(parent.claim_expires_at_unix_ms.unwrap() > now_ms());
}

#[test]
fn routine_lease_renewal_updates_local_projection_without_replica_envelope() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("renewal.sqlite3");
    let store = Store::open(&path, "node").unwrap();
    let source = "version 2\nmission \"renewal\" state=\"ready\" { goal \"Hold work.\"; step \"work\" { assigned-to \"agent/node.worker\" } }";
    let intent = crate::graph::parse_test_intent(source, "node").unwrap();
    let plan = store
        .mission(
            &intent,
            IntentInput {
                kdl: source.into(),
                source_name: None,
            },
        )
        .unwrap();
    store
        .apply(&intent, &plan.subject_tokens, "renewal-mission")
        .unwrap();
    let run = store
        .create_mission_run(&MissionRunRequest {
            mission: "renewal".into(),
            revision: None,
            workspace: "/tmp".into(),
            requester: Some("person/test".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "renewal-run".into(),
        })
        .unwrap();
    let subject = &run.steps[0].subject;
    store.set_step_state(subject, "ready", None).unwrap();
    let request = |key: &str| WorkRequest {
        wait_ms: None,
        actor: Some("agent/node.worker".into()),
        incarnation: Some("current".into()),
        summary: None,
        reason: None,
        evidence: Vec::new(),
        idempotency_key: key.into(),
    };
    let claimed = store
        .work_action(subject, "claim", &request("renewal-claim"))
        .unwrap();
    let inventory_before = store.replication_inventory().unwrap().envelopes.len();
    std::thread::sleep(std::time::Duration::from_millis(2));
    let renewed = store
        .work_action(subject, "renew", &request("renewal-first"))
        .unwrap();
    assert!(renewed.claim_expires_at_unix_ms > claimed.claim_expires_at_unix_ms);
    assert_eq!(
        store.replication_inventory().unwrap().envelopes.len(),
        inventory_before
    );
    assert!(
        store
            .claims_for(subject, Some("work.renewed"))
            .unwrap()
            .is_empty()
    );
    let replica = Store::open_memory("replica").unwrap();
    let exchange = exchange_from(&store, &ReplicationInventory::default());
    receive_and_project(&replica, "node", &exchange);
    let remote_expiry = replica
        .step_run(subject)
        .unwrap()
        .unwrap()
        .claim_expires_at_unix_ms
        .unwrap();
    assert_eq!(Some(remote_expiry), claimed.claim_expires_at_unix_ms);
    assert!(remote_expiry > now_ms() + 300_000);
    assert!(store.project_replication_backlog().unwrap());
    assert_eq!(
        store
            .step_run(subject)
            .unwrap()
            .unwrap()
            .claim_expires_at_unix_ms,
        renewed.claim_expires_at_unix_ms,
        "a projection replay must retain the local renewal"
    );
    drop(store);
    let reopened = Store::open(&path, "node").unwrap();
    assert_eq!(
        reopened
            .step_run(subject)
            .unwrap()
            .unwrap()
            .claim_expires_at_unix_ms,
        renewed.claim_expires_at_unix_ms
    );
    let before_material = reopened.replication_inventory().unwrap().envelopes.len();
    let mut material = request("renewal-material");
    material.summary = Some("Made progress on the review".into());
    reopened.work_action(subject, "renew", &material).unwrap();
    assert_eq!(
        reopened.replication_inventory().unwrap().envelopes.len(),
        before_material + 1
    );
    assert_eq!(
        reopened.claims_for(subject, Some("work.renewed")).unwrap()[0].body["fields"]["summary"],
        "Made progress on the review"
    );
}

fn timeline_observation(subject: &str, incarnation: &str, entry: &str) -> ClaimInput {
    ClaimInput {
        subject: subject.into(),
        kind: "harness.timeline".into(),
        actor: Some(subject.into()),
        fields: BTreeMap::from([
            ("operation".into(), Value::String("append".into())),
            ("entry_id".into(), Value::String(entry.into())),
            ("revision".into(), Value::from(1)),
            ("role".into(), Value::String("assistant".into())),
            ("entry_type".into(), Value::String("content".into())),
            ("final".into(), Value::Bool(true)),
            (
                "body".into(),
                json!({"media_type": "text/plain", "text": entry}),
            ),
            ("driver".into(), Value::String("codex".into())),
            ("incarnation_id".into(), Value::String(incarnation.into())),
            ("sequence".into(), Value::from(entry.len() as u64)),
        ]),
        evidence: Vec::new(),
        expected_subject: None,
        idempotency_key: Some(format!("timeline:{subject}:{incarnation}:{entry}")),
    }
}

#[test]
fn agent_activity_ignores_timeline_status_and_tracks_content() {
    let store = Store::open_memory("node").unwrap();
    let agent = "agent/node.worker";
    let mut status = timeline_observation(agent, "inc-1", "idle");
    status
        .fields
        .insert("entry_type".into(), Value::String("status".into()));
    store.append_claim(&status).unwrap();
    assert_eq!(
        store
            .agent_last_activity_at(agent, Some("inc-1"), store.index().unwrap())
            .unwrap(),
        None
    );
    let content = store
        .append_claim(&timeline_observation(agent, "inc-1", "answer"))
        .unwrap();
    assert_eq!(
        store
            .agent_last_activity_at(agent, Some("inc-1"), store.index().unwrap())
            .unwrap(),
        Some(content.accepted_at_unix_ms)
    );
    let mut later_status = timeline_observation(agent, "inc-1", "working");
    later_status
        .fields
        .insert("entry_type".into(), Value::String("status".into()));
    store.append_claim(&later_status).unwrap();
    assert_eq!(
        store
            .agent_last_activity_at(agent, Some("inc-1"), store.index().unwrap())
            .unwrap(),
        Some(content.accepted_at_unix_ms)
    );
}

#[test]
fn agent_silence_starts_at_the_latest_working_transition() {
    let store = Store::open_memory("node").unwrap();
    let agent = "agent/node.worker";
    let observe = |state: &str, key: &str| {
        store
            .append_claim(&ClaimInput {
                subject: agent.into(),
                kind: "harness.observed".into(),
                actor: Some(agent.into()),
                fields: BTreeMap::from([
                    ("state".into(), Value::String(state.into())),
                    ("driver".into(), Value::String("codex".into())),
                    ("incarnation_id".into(), Value::String("inc-1".into())),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some(key.into()),
            })
            .unwrap()
    };
    observe("idle", "idle-one");
    let first = observe("working", "work-one");
    observe("working", "work-heartbeat");
    assert_eq!(
        store
            .agent_working_since(agent, "inc-1", store.index().unwrap())
            .unwrap(),
        Some(first.accepted_at_unix_ms)
    );
    observe("idle", "idle-two");
    let second = observe("working", "work-two");
    assert_eq!(
        store
            .agent_working_since(agent, "inc-1", store.index().unwrap())
            .unwrap(),
        Some(second.accepted_at_unix_ms)
    );
}

#[test]
fn working_episode_reverse_fold_matches_full_canonical_history() {
    let store = Store::open_memory("working-episode").unwrap();
    let agent = "agent/working-episode";
    let mut ids = Vec::new();
    for (i, state) in ["working", "idle", "working", "working", "idle", "working"]
        .into_iter()
        .enumerate()
    {
        ids.push(
            store
                .append_claim(&ClaimInput {
                    subject: agent.into(),
                    kind: "harness.observed".into(),
                    actor: Some(agent.into()),
                    fields: BTreeMap::from([
                        ("state".into(), json!(state)),
                        ("driver".into(), json!("codex")),
                        ("incarnation_id".into(), json!("inc-1")),
                        ("reason".into(), json!(format!("episode-observation-{i}"))),
                    ]),
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: Some(format!("working-episode-{i}")),
                })
                .unwrap(),
        );
    }
    assert_eq!(
        ids.iter()
            .map(|claim| &claim.id)
            .collect::<BTreeSet<_>>()
            .len(),
        6
    );
    // Invented timestamp ties and reversed times deliberately disagree with arrival.
    // Keep actual Store admission and the independent old SQL fold as separate oracles.
    {
        let connection = store.connection.write();
        for (claim, time) in ids.iter().zip(["9", "10", "10", "12", "11", "11"]) {
            assert_eq!(
                connection
                    .execute(
                        "UPDATE claims SET accepted_at_unix_ms=?1 WHERE id=?2",
                        params![time, claim.id],
                    )
                    .unwrap(),
                1,
                "each observation ID must name one persisted claim"
            );
        }
    }
    let connection = store.readers.get();
    let oracle = |cut| {
        let mut statement = connection
            .prepare(&format!(
                "SELECT json_extract(claims.body, '$.fields.state'), claims.accepted_at_unix_ms
                 FROM claims JOIN batches ON batches.id=claims.batch_id
                 WHERE claims.subject=?1 AND claims.kind='harness.observed'
                   AND json_extract(claims.body, '$.fields.incarnation_id')=?2
                   AND claims.store_index<=?3 ORDER BY {CANONICAL_ORDER}"
            ))
            .unwrap();
        let states = statement
            .query_map(params![agent, "inc-1", cut], |row| {
                Ok((row.get::<_, Option<String>>(0)?, row.get::<_, String>(1)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        let after = states
            .iter()
            .rposition(|(state, _)| state.as_deref().is_some_and(|state| state != "working"))
            .map_or(0, |p| p + 1);
        states[after..]
            .iter()
            .find(|(state, _)| state.as_deref() == Some("working"))
            .and_then(|(_, time)| time.parse::<u128>().ok())
    };
    for claim in &ids {
        assert_eq!(
            agent_working_since_at(&connection, agent, "inc-1", claim.store_index).unwrap(),
            oracle(claim.store_index)
        );
    }
    assert_eq!(
        agent_working_since_at(&connection, agent, "other-incarnation", u64::MAX >> 1).unwrap(),
        None
    );
    assert_eq!(
        agent_working_since_at(&connection, "agent/missing", "inc-1", u64::MAX >> 1).unwrap(),
        None
    );
    drop(connection);
    {
        let writer = store.connection.write();
        writer
            .execute(
                "UPDATE claims SET body=json_remove(body,'$.fields.state') WHERE id=?1",
                [&ids[5].id],
            )
            .unwrap();
    }
    let connection = store.readers.get();
    assert_eq!(
        agent_working_since_at(&connection, agent, "inc-1", ids[5].store_index).unwrap(),
        Some(12)
    );
    drop(connection);
    {
        let writer = store.connection.write();
        writer
            .execute(
                "UPDATE claims SET body=json_extract(body,'$.fields') WHERE id=?1",
                [&ids[3].id],
            )
            .unwrap();
    }
    // The incarnation index contains this flat observation, but the old fields-only
    // predicate must still exclude it. Null state does not restart a prior episode.
    let connection = store.readers.get();
    assert_eq!(
        agent_working_since_at(&connection, agent, "inc-1", ids[5].store_index).unwrap(),
        None
    );
    assert!(connection.is_autocommit());
    connection
        .execute_batch("PRAGMA query_only=ON; BEGIN")
        .unwrap();
    assert_eq!(
        agent_working_since_at(&connection, agent, "inc-1", ids[5].store_index).unwrap(),
        None
    );
    assert!(!connection.is_autocommit());
    connection
        .execute_batch(
            "ROLLBACK; PRAGMA query_only=OFF; DROP INDEX claims_incarnation_accepted_index",
        )
        .unwrap();
    assert!(agent_working_since_at(&connection, agent, "inc-1", ids[5].store_index).is_err());
    assert!(connection.is_autocommit());
    connection
        .execute_batch("PRAGMA query_only=ON; BEGIN")
        .unwrap();
    assert!(agent_working_since_at(&connection, agent, "inc-1", ids[5].store_index).is_err());
    assert!(!connection.is_autocommit());
    connection
        .execute_batch("ROLLBACK; PRAGMA query_only=OFF")
        .unwrap();
}

#[test]
#[ignore = "process-wide SQL counters: run alone with --exact --ignored --test-threads=1"]
fn working_episode_reverse_seek_ignores_completed_history_growth() {
    use smallclaims::sqlite::work;
    let measure = |old_count: u64, completed_episode: bool, tied_tail: bool| {
        let store = Store::open_memory("episode-growth").unwrap();
        let mut connection = store.connection.write();
        let tx = connection.transaction().unwrap();
        tx.execute(
            "INSERT INTO batches(id,origin,replica_sequence,hash,accepted_at_unix_ms)
                VALUES ('episode-batch','episode-growth',1,'synthetic','1')",
            [],
        )
        .unwrap();
        for index in (1..=old_count).chain(100000..=100003) {
            let state = match index {
                100001 | 100003 => json!("working"),
                100002 => Value::Null,
                100000 if !completed_episode => Value::Null,
                _ if !completed_episode => json!("working"),
                _ => json!("idle"),
            };
            let body =
                json!({"fields":{"state":state,"driver":"codex","incarnation_id":"inc-1"}});
            tx.execute("INSERT INTO claims(store_index,id,batch_id,subject,kind,origin,body,predecessors,accepted_at_unix_ms)
                    VALUES (?1,?2,'episode-batch','agent/episode-growth','harness.observed','episode-growth',?3,'[]',?4)",
                params![index, format!("synthetic-episode-{index}"), body.to_string(), if tied_tail && index==100003 {"100001".to_owned()} else {index.to_string()}]).unwrap();
        }
        tx.commit().unwrap();
        let query = working_episode_query();
        let plans = connection
            .prepare(&format!("EXPLAIN QUERY PLAN {query}"))
            .unwrap()
            .query_map(params!["agent/episode-growth", "inc-1", 100003], |row| {
                row.get::<_, String>(3)
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert!(plans.iter().any(|p| p.contains("SEARCH claims USING INDEX claims_incarnation_accepted_index (subject=? AND <expr>=?)")), "{plans:?}");
        let before = work::total();
        assert_eq!(
            agent_working_since_at(&connection, "agent/episode-growth", "inc-1", 100003)
                .unwrap(),
            Some(if completed_episode { 100001 } else { 1 })
        );
        let work = work::total() - before;
        assert!(work.statements > 0 && work.vm_steps > 0);
        println!(
            "working episode synthetic production-schema old_count={old_count} completed_episode={completed_episode} tied_tail={tied_tail} work={work:?} plan={plans:?}"
        );
        work
    };
    let small = measure(1000, true, false);
    let large = measure(10000, true, false);
    assert!(
        small.vm_steps > 0 && large.vm_steps <= small.vm_steps + 100,
        "small={small:?} large={large:?}"
    );
    assert_eq!(small.fullscan_steps, 0);
    assert_eq!(large.fullscan_steps, 0);
    let long_small = measure(1000, false, false);
    let long_large = measure(10000, false, false);
    assert!(
        long_large.vm_steps > long_small.vm_steps * 5,
        "an uninterrupted episode still grows: {long_small:?} {long_large:?}"
    );
    let tie_small = measure(1000, true, true);
    let tie_large = measure(10000, true, true);
    assert!(
        tie_large.vm_steps > tie_small.vm_steps * 5,
        "recordless canonical ties still read batch rank history: {tie_small:?} {tie_large:?}"
    );
}

fn timeline_entries(page: &ClaimsPage) -> Vec<String> {
    page.claims
        .iter()
        .map(|claim| {
            claim.body["fields"]["entry_id"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect()
}

#[test]
fn a_local_timeline_observation_stays_on_the_node_that_made_it() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("local.sqlite3");
    let store = Store::open(&path, "node").unwrap();
    let subject = "agent/node.worker";
    let index_before = store.index().unwrap();
    let inventory_before = store.replication_inventory().unwrap();

    let mut written = Vec::new();
    for entry in ["first", "second", "third"] {
        let (record, appended) = store
            .append_claim_outcome(&timeline_observation(subject, "inc-1", entry))
            .unwrap();
        assert!(appended);
        assert!(
            record.id.starts_with("local-observation/node/"),
            "{}",
            record.id
        );
        assert_eq!(record.store_index, index_before);
        written.push(record);
    }
    let (repeated, appended) = store
        .append_claim_outcome(&timeline_observation(subject, "inc-1", "second"))
        .unwrap();
    assert!(!appended, "a repeated idempotency key records nothing new");
    assert_eq!(repeated.id, written[1].id);
    let mut changed = timeline_observation(subject, "inc-1", "second");
    changed.fields.insert(
        "body".into(),
        json!({"media_type": "text/plain", "text": "a different body"}),
    );
    assert_eq!(
        store.append_claim_outcome(&changed).unwrap_err().code,
        "idempotency-mismatch",
        "a retry with a changed body is not silently dropped"
    );
    let mut elsewhere = timeline_observation("agent/node.other", "inc-1", "second");
    elsewhere.idempotency_key =
        timeline_observation(subject, "inc-1", "second").idempotency_key;
    assert_eq!(
        store.append_claim_outcome(&elsewhere).unwrap_err().code,
        "idempotency-mismatch",
        "a key reused for another subject does not return the first subject's row"
    );
    let mut unkeyed = timeline_observation(subject, "inc-1", "second");
    unkeyed.idempotency_key = None;
    assert!(
        store.append_claim_outcome(&unkeyed).unwrap().1,
        "an observation without a key is always new"
    );

    assert_eq!(store.index().unwrap(), index_before, "no claim was written");
    assert!(
        store
            .claims_for(subject, Some("harness.timeline"))
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store.replication_inventory().unwrap().envelopes,
        inventory_before.envelopes,
        "a local observation adds no envelope"
    );
    let page = store
        .timeline_claims_for_incarnation_at(subject, "inc-1", None, false, 10)
        .unwrap();
    assert_eq!(
        timeline_entries(&page),
        ["first", "second", "third", "second"]
    );
    assert_eq!(page.next_cursor, None);

    let replica = Store::open_memory("replica").unwrap();
    let exchange = exchange_from(&store, &ReplicationInventory::default());
    receive_and_project(&replica, "node", &exchange);
    assert!(
        replica
            .timeline_claims_for_incarnation_at(subject, "inc-1", None, false, 10)
            .unwrap()
            .claims
            .is_empty(),
        "a peer never receives a local observation"
    );

    drop(store);
    let reopened = Store::open(&path, "node").unwrap();
    let page = reopened
        .timeline_claims_for_incarnation_at(subject, "inc-1", None, false, 10)
        .unwrap();
    assert_eq!(
        timeline_entries(&page),
        ["first", "second", "third", "second"]
    );
    let mut with_head = timeline_observation(subject, "inc-1", "fourth");
    with_head.expected_subject = Some(None);
    assert_eq!(
        reopened.append_claim_outcome(&with_head).unwrap_err().code,
        "local-observation-head"
    );
    let mut missing = timeline_observation(subject, "inc-1", "fifth");
    missing.fields.remove("incarnation_id");
    assert_eq!(
        reopened.append_claim_outcome(&missing).unwrap_err().code,
        "missing-claim-field"
    );
    let mut unnumbered = timeline_observation(subject, "inc-1", "sixth");
    unnumbered.fields.remove("sequence");
    assert_eq!(
        reopened.append_claim_outcome(&unnumbered).unwrap_err().code,
        "missing-claim-field"
    );
}

#[test]
fn response_usage_stays_local_while_attributed_totals_replicate() {
    let local = Store::open_memory("host-one").unwrap();
    let subject = "agent/example.worker";
    let seed = local
        .append_claim(&ClaimInput {
            subject: subject.into(),
            kind: "harness.observed".into(),
            actor: Some(subject.into()),
            fields: BTreeMap::from([
                ("state".into(), Value::String("ready".into())),
                ("driver".into(), Value::String("claude".into())),
                ("incarnation_id".into(), Value::String("inc-one".into())),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some("usage-seed".into()),
        })
        .unwrap();
    local.connection.lock().unwrap().execute(
        "INSERT INTO desired(subject, kind, revision, claim_id, body, owner_run, owner_generation, owner_step)
             VALUES (?1, 'agent', 'revision', ?2, '{}', 'mission-run/example', 'run-generation/example', 'step-run/example/record')",
        params![subject, seed.id],
    ).unwrap();
    let record = |entry: &str, input: u64, output: u64, reads: u64, writes: u64| {
        let mut claim = timeline_observation(subject, "inc-one", entry);
        claim
            .fields
            .insert("entry_type".into(), Value::String("usage".into()));
        claim
            .fields
            .insert("role".into(), Value::String("system".into()));
        claim
            .fields
            .insert("driver".into(), Value::String("claude".into()));
        claim.fields.insert(
            "body".into(),
            json!({
                "semantics": "response", "model": "claude-example", "turn_id": entry,
                "input_tokens": input, "output_tokens": output,
                "cached_tokens": reads, "cache_write_tokens": writes,
                "total_tokens": input + output + reads + writes,
            }),
        );
        claim
    };
    let first = record("turn-a", 4, 2, 20, 3);
    let (observation, appended) = local.append_claim_outcome(&first).unwrap();
    assert!(appended);
    assert_eq!(
        observation.body["fields"]["attribution"]["mission_run_id"],
        "mission-run/example"
    );
    assert_eq!(
        observation.body["fields"]["attribution"]["step_id"],
        "step-run/example/record"
    );
    let rollup = local
        .usage_rollup_for_timeline(&observation)
        .unwrap()
        .unwrap();
    assert_eq!(rollup.fields["total_tokens"], 29);
    assert_eq!(rollup.fields["host"], "host-one");
    local.append_claim(&rollup).unwrap();
    let baseline = rollup.fields["observed_at_unix_ms"].as_u64().unwrap();
    let (repeated, appended) = local.append_claim_outcome(&first).unwrap();
    assert!(!appended);
    assert_eq!(
        local
            .usage_rollup_for_timeline(&repeated)
            .unwrap()
            .unwrap()
            .fields["total_tokens"],
        29
    );
    std::thread::sleep(std::time::Duration::from_millis(2));
    let mut second = record("turn-b", 5, 1, 30, 0);
    second
        .fields
        .insert("source_id".into(), Value::String("same-response".into()));
    let (observation, appended) = local.append_claim_outcome(&second).unwrap();
    assert!(appended);
    let rollup = local
        .usage_rollup_for_timeline(&observation)
        .unwrap()
        .unwrap();
    assert_eq!(rollup.fields["total_tokens"], 65);
    local.append_claim(&rollup).unwrap();
    let mut replay = second.clone();
    replay
        .fields
        .insert("incarnation_id".into(), Value::String("inc-two".into()));
    replay.idempotency_key = Some("replayed-after-restart".into());
    let (second_replay, _) = local.append_claim_outcome(&replay).unwrap();
    assert!(
        local
            .usage_rollup_for_timeline(&second_replay)
            .unwrap()
            .is_none()
    );
    let period = local
        .usage_period_rows(baseline, baseline + 60_000)
        .unwrap();
    assert_eq!(period.len(), 1);
    assert_eq!(period[0]["total_tokens"], 36);
    assert_eq!(period[0]["cached_tokens"], 30);
    assert_eq!(
        local
            .usage_summary_at(subject, None, None)
            .unwrap()
            .unwrap()
            .total_tokens,
        65
    );

    let peer = Store::open_memory("host-two").unwrap();
    let exchange = exchange_from(&local, &ReplicationInventory::default());
    receive_and_project(&peer, "host-one", &exchange);
    assert!(
        peer.timeline_claims_for_incarnation_at(subject, "inc-one", None, false, 10)
            .unwrap()
            .claims
            .is_empty()
    );
    assert_eq!(
        peer.usage_period_rows(baseline, baseline + 60_000).unwrap(),
        period
    );
    assert_eq!(
        peer.usage_summary_at(subject, None, None)
            .unwrap()
            .unwrap()
            .total_tokens,
        65
    );
}

#[test]
fn usage_provenance_survives_resume_replay_periods_and_reopen() {
    let scratch = tempfile::tempdir().unwrap();
    let path = scratch.path().join("usage.sqlite3");
    let local = Store::open(&path, "host-one").unwrap();
    let subject = "agent/example.provenance";
    let binding = |incarnation: &str, session: &str| {
        local
            .append_claim(&ClaimInput {
                subject: subject.into(),
                kind: "harness.session-file".into(),
                actor: Some(subject.into()),
                fields: BTreeMap::from([
                    ("harness".into(), json!("codex")),
                    ("session_id".into(), json!(session)),
                    ("incarnation_id".into(), json!(incarnation)),
                ]),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: Some(format!("binding-{incarnation}")),
            })
            .unwrap();
    };
    let respond = |incarnation: &str, entry: &str, input: u64, reported: bool| {
        let mut claim = timeline_observation(subject, incarnation, entry);
        claim.fields.insert("driver".into(), json!("codex"));
        claim.fields.insert("entry_type".into(), json!("usage"));
        let mut body = json!({"semantics":"response", "model":"gpt-6.1-sol",
            "input_tokens":input,"output_tokens":100,"total_tokens":input+100});
        if reported {
            body["cost"] = json!(0.001);
        }
        claim.fields.insert("body".into(), body);
        let observation = local.append_claim(&claim).unwrap();
        let rollup = local
            .usage_rollup_for_timeline(&observation)
            .unwrap()
            .unwrap();
        local.append_client_claim(&rollup).unwrap();
        (claim, observation, rollup)
    };
    binding("inc-one", "native-example");
    let (first, observation, baseline) = respond("inc-one", "price-short", 1000, false);
    let provenance = &baseline.fields["pricing_provenance"][0];
    assert_eq!(provenance["price_table_id"], crate::pricing::PRICE_TABLE_ID);
    assert_eq!(
        provenance["price_table_version"],
        crate::pricing::price_table_version()
    );
    assert_eq!(provenance["cost_source"], "computed");
    assert_eq!(provenance["rates_usd_per_million_tokens"]["input"], 2.0);
    assert_eq!(baseline.fields["native_session_id"], "native-example");
    let since = baseline.fields["observed_at_unix_ms"].as_u64().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(2));
    respond("inc-one", "price-long", 300000, false);
    let (_, _, mixed) = respond("inc-one", "price-reported", 10, true);
    assert_eq!(
        mixed.fields["pricing_provenance"].as_array().unwrap().len(),
        3
    );
    let computed = mixed.fields["pricing_provenance"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|p| p["cost_source"] == "computed")
        .collect::<Vec<_>>();
    assert!(
        computed
            .iter()
            .any(|p| p["rates_usd_per_million_tokens"]["input"] == 4.0
                && p["rates_usd_per_million_tokens"]["output"] == 15.0)
    );
    let reported = mixed.fields["pricing_provenance"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["cost_source"] == "provider_reported")
        .unwrap();
    assert!(reported.get("rates_usd_per_million_tokens").is_none());
    assert!(reported.get("price_table_id").is_none());
    // Replaying the response never adds another provenance contribution.
    local.append_claim(&first).unwrap();
    assert_eq!(
        local
            .usage_rollup_for_timeline(&observation)
            .unwrap()
            .unwrap()
            .fields,
        mixed.fields
    );
    let period = local.usage_period_rows(since, u64::MAX).unwrap();
    assert_eq!(period[0]["pricing_provenance"].as_array().unwrap().len(), 2);
    assert_eq!(period[0]["total_tokens"], 300210);
    assert_eq!(
        period[0]["pricing_provenance"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["total_tokens"].as_u64().unwrap())
            .sum::<u64>(),
        300210
    );
    let typed: st3_client::UsageRow = serde_json::from_value(period[0].clone()).unwrap();
    assert_eq!(typed.native_session_id.as_deref(), Some("native-example"));
    assert_eq!(typed.pricing_provenance.unwrap().len(), 2);
    // Resume binds a new incarnation to the same conversation. A newer conversation on
    // this seat must not steal the old incarnation's spend.
    binding("inc-two", "native-example");
    let (_, _, resumed) = respond("inc-two", "after-resume", 20, false);
    assert_eq!(resumed.fields["native_session_id"], "native-example");
    binding("inc-three", "native-other");
    assert_eq!(
        local
            .usage_rollup_for_timeline(&observation)
            .unwrap()
            .unwrap()
            .fields["native_session_id"],
        "native-example"
    );
    let all = local.usage_period_rows(0, u64::MAX).unwrap();
    let peer = Store::open_memory("host-two").unwrap();
    receive_and_project(
        &peer,
        "host-one",
        &exchange_from(&local, &ReplicationInventory::default()),
    );
    assert_eq!(peer.usage_period_rows(0, u64::MAX).unwrap(), all);
    drop(local);
    let reopened = Store::open(&path, "host-one").unwrap();
    assert_eq!(reopened.usage_period_rows(0, u64::MAX).unwrap(), all);
    assert_eq!(
        reopened
            .usage_rollup_for_timeline(&observation)
            .unwrap()
            .unwrap()
            .fields,
        mixed.fields
    );
}

#[test]
fn usage_period_keeps_price_versions_separate_without_repricing_the_baseline() {
    let local = Store::open_memory("host-one").unwrap();
    let subject = "agent/example.price-change";
    let contribution = |version: &str, tokens: u64, rate: u64| {
        json!({
            "price_table_id":"st.api-list", "price_table_version":version,
            "cost_source":"computed",
            "rates_usd_per_million_tokens":{"input":rate,"output":rate,"cache_read":rate,
                "cache_write_5m":rate,"cache_write_1h":rate},
            "total_tokens":tokens,"input_tokens":tokens,"output_tokens":0,
            "cache_write_tokens":0,"cache_write_1h_tokens":0,"cached_tokens":0,
            "cost_microusd":tokens*rate,"reported_cost_microusd":0,"unpriced_tokens":0,
        })
    };
    let old = contribution("example-v1", 10, 1);
    let new = contribution("example-v2", 20, 2);
    for (at, total, cost, provenance) in [
        (10, 10, 10, json!([old.clone()])),
        (20, 30, 50, json!([old, new])),
    ] {
        local
            .append_client_claim(&ClaimInput {
                subject: subject.into(),
                kind: "harness.usage".into(),
                actor: Some(subject.into()),
                fields: serde_json::from_value(json!({
                    "semantics":"response_rollup","driver":"codex","incarnation_id":"inc-one",
                    "model":"example-model","native_session_id":"native-example",
                    "observed_at_unix_ms":at,"total_tokens":total,"input_tokens":total,
                    "cost_microusd":cost,"pricing_provenance":provenance,
                }))
                .unwrap(),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
    }
    let rows = local.usage_period_rows(10, 20).unwrap();
    let (reported_rows, estimate) = local.usage_period_report(10, 20).unwrap();
    assert_eq!(reported_rows, rows);
    assert_eq!(estimate["days"][0]["usage_cost_microusd"], 40);
    assert_eq!(rows[0]["total_tokens"], 20);
    assert_eq!(rows[0]["cost_microusd"], 40);
    let provenance = rows[0]["pricing_provenance"].as_array().unwrap();
    assert_eq!(provenance.len(), 1);
    assert_eq!(provenance[0]["price_table_version"], "example-v2");
    assert_eq!(provenance[0]["rates_usd_per_million_tokens"]["input"], 2);
    assert_eq!(provenance[0]["total_tokens"], 20);
    assert_eq!(provenance[0]["cost_microusd"], 40);
    let lifetime = local.usage_period_rows(0, 20).unwrap();
    assert_eq!(
        lifetime[0]["pricing_provenance"].as_array().unwrap().len(),
        2
    );
    assert_eq!(lifetime[0]["cost_microusd"], 50);
}

#[test]
fn historical_usage_claims_leave_provenance_unknown() {
    let local = Store::open_memory("host-one").unwrap();
    let subject = "agent/example.legacy";
    local
        .append_client_claim(&ClaimInput {
            subject: subject.into(),
            kind: "harness.usage".into(),
            actor: Some(subject.into()),
            fields: BTreeMap::from([
                ("semantics".into(), json!("response_rollup")),
                ("driver".into(), json!("claude")),
                ("incarnation_id".into(), json!("legacy-inc")),
                ("total_tokens".into(), json!(99)),
                ("observed_at_unix_ms".into(), json!(10)),
                ("pricing".into(), json!("old-label")),
            ]),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    let rows = local.usage_period_rows(0, 100).unwrap();
    assert_eq!(rows[0]["unpriced_tokens"], 99);
    assert_eq!(rows[0]["pricing"], "old-label");
    assert!(rows[0].get("pricing_provenance").is_none());
    assert!(rows[0].get("native_session_id").is_none());
    let typed: st3_client::UsageRow = serde_json::from_value(rows[0].clone()).unwrap();
    assert!(typed.native_session_id.is_none());
    assert!(typed.pricing_provenance.is_none());
}

#[test]
fn response_spend_is_priced_and_kept_apart_by_account_and_step() {
    let local = Store::open_memory("host-one").unwrap();
    let subject = "agent/example.worker";
    let observed = |state: &str, key: &str| {
        local
            .append_claim(&ClaimInput {
                subject: subject.into(),
                kind: "harness.observed".into(),
                actor: Some(subject.into()),
                fields: BTreeMap::from([
                    ("state".into(), Value::String(state.into())),
                    ("driver".into(), Value::String("claude".into())),
                    ("incarnation_id".into(), Value::String("inc-one".into())),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some(key.into()),
            })
            .unwrap()
    };
    let seed = observed("working", "spend-seed");
    let own_step = |step: &str| {
        local.connection.lock().unwrap().execute(
            "INSERT OR REPLACE INTO desired(subject, kind, revision, claim_id, body, owner_run, owner_generation, owner_step)
                 VALUES (?1, 'agent', 'revision', ?2, '{}', 'mission-run/example', 'run-generation/example', ?3)",
            params![subject, seed.id, step],
        ).unwrap();
    };
    let respond = |entry: &str, body: Value| {
        let mut claim = timeline_observation(subject, "inc-one", entry);
        claim
            .fields
            .insert("entry_type".into(), Value::String("usage".into()));
        claim
            .fields
            .insert("role".into(), Value::String("system".into()));
        claim
            .fields
            .insert("driver".into(), Value::String("claude".into()));
        claim.fields.insert("body".into(), body);
        let (observation, appended) = local.append_claim_outcome(&claim).unwrap();
        assert!(appended);
        let rollup = local
            .usage_rollup_for_timeline(&observation)
            .unwrap()
            .unwrap();
        local.append_client_claim(&rollup).unwrap();
        rollup
    };
    let opus = |account: &str| {
        json!({"semantics": "response", "model": "claude-opus-5-5", "account": account,
            "input_tokens": 1000, "output_tokens": 100, "cached_tokens": 10000,
            "cache_write_tokens": 2000, "cache_write_1h_tokens": 1000, "total_tokens": 13100})
    };
    let start = now_ms() as u64;
    own_step("step-run/example/build");
    let build = respond("response-a", opus("claude/aaaaaaaaaaaaaaaa"));
    // $4 input, $20 output, $0.20 cache read, $5 and $8 for five-minute and one-hour writes.
    assert_eq!(
        build.fields["cost_microusd"],
        4_000 + 2_000 + 2_000 + 5_000 + 8_000
    );
    assert_eq!(build.fields["account"], "claude/aaaaaaaaaaaaaaaa");
    assert_eq!(build.fields["pricing"], crate::pricing::PRICING_REVISION);
    own_step("step-run/example/review");
    respond("response-b", opus("claude/bbbbbbbbbbbbbbbb"));
    respond(
        "response-c",
        json!({"semantics": "response", "model": "local-example", "input_tokens": 70, "output_tokens": 7, "total_tokens": 77}),
    );
    respond(
        "response-d",
        json!({"semantics": "response", "model": "local-example", "input_tokens": 9, "output_tokens": 1, "total_tokens": 10, "cost": 0.0025, "currency": "USD"}),
    );

    // Each key replicates even while the harness keeps working: a step change must not
    // leave the previous step's last totals pending behind the next step's.
    let replicated = local.claims_for(subject, Some("harness.usage")).unwrap();
    assert_eq!(replicated.len(), 3, "{replicated:#?}");
    // The local model's second response waits for the next interval, or for the harness
    // to stop working.
    observed("ready", "spend-idle");
    assert_eq!(
        local
            .claims_for(subject, Some("harness.usage"))
            .unwrap()
            .len(),
        4
    );

    let rows = local
        .usage_period_rows(start.saturating_sub(1), now_ms() as u64 + 1)
        .unwrap();
    let row = |step: &str, account: &str, model: &str| {
        rows.iter()
            .find(|row| {
                row["step"] == step && row["account"] == account && row["model"] == model
            })
            .unwrap_or_else(|| panic!("{step} {account} {model}: {rows:#?}"))
    };
    let build = row(
        "step-run/example/build",
        "claude/aaaaaaaaaaaaaaaa",
        "claude-opus-5-5",
    );
    assert_eq!(build["cost_microusd"], 21_000);
    assert_eq!(build["unpriced_tokens"], 0);
    let review = row(
        "step-run/example/review",
        "claude/bbbbbbbbbbbbbbbb",
        "claude-opus-5-5",
    );
    assert_eq!(review["cost_microusd"], 21_000);
    assert_eq!(review["mission_run"], "mission-run/example");
    let local_model = row("step-run/example/review", "", "local-example");
    assert_eq!(local_model["total_tokens"], 87);
    assert_eq!(
        local_model["unpriced_tokens"], 77,
        "an unpriced response is never free"
    );
    assert_eq!(local_model["cost_microusd"], 2_500);
    assert_eq!(local_model["reported_cost_microusd"], 2_500);
}

#[test]
fn response_usage_binds_to_the_step_held_when_the_harness_recorded_it() {
    let store = Store::open_memory("host-one").unwrap();
    let subject = "agent/example.worker";
    let activated = now_ms() as u64 - 60_000;
    {
        let connection = store.connection.lock().unwrap();
        connection
            .execute_batch("PRAGMA foreign_keys=OFF;")
            .unwrap();
        connection.execute(
            "INSERT INTO step_runs(subject, run_id, generation_id, step_path, definition_hash, status, attempt,
                    goals, lease_owner, lease_incarnation, lease_expires_at_unix_ms, activated_at_unix_ms,
                    created_at_unix_ms, updated_at_unix_ms)
                 VALUES ('step-run/example/build', 'example', 'example', 'build', 'hash', 'working', 1,
                    '[]', ?1, 'inc-one', ?2, ?3, ?3, ?3)",
            params![subject, (activated + 3_600_000).to_string(), activated.to_string()],
        ).unwrap();
        connection.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
    }
    let respond = |entry: &str, at: u64| {
        let mut claim = timeline_observation(subject, "inc-one", entry);
        claim
            .fields
            .insert("entry_type".into(), Value::String("usage".into()));
        claim
            .fields
            .insert("observed_at_unix_ms".into(), Value::from(at));
        claim.fields.insert(
            "body".into(),
            json!({"semantics": "response", "model": "claude-opus-5-5", "input_tokens": 1, "output_tokens": 1, "total_tokens": 2}),
        );
        store.append_claim_outcome(&claim).unwrap().0.body["fields"]["attribution"].clone()
    };
    let during = respond("during", activated + 1);
    assert_eq!(during["step_id"], "step-run/example/build");
    assert_eq!(during["mission_run_id"], "mission-run/example");
    // Replayed later, a response from before the step began is not charged to it.
    let before = respond("before", activated - 1);
    assert!(before["step_id"].is_null(), "{before}");
}

#[test]
fn counted_responses_are_remembered_for_a_bounded_horizon() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("claims.sqlite3");
    {
        let store = Store::open(&path, "host-one").unwrap();
        // An older build kept every counted response forever, without a date.
        store
            .connection
            .lock()
            .unwrap()
            .execute_batch(
                "CREATE TABLE local_usage_seen (subject TEXT NOT NULL, source_id TEXT NOT NULL,
                        PRIMARY KEY(subject, source_id));
                     INSERT INTO local_usage_seen VALUES ('agent/example.worker', 'counted-before');",
            )
            .unwrap();
    }
    let store = Store::open(&path, "host-one").unwrap();
    let subject = "agent/example.worker";
    let respond = |entry: &str, at: u128| {
        let mut claim = timeline_observation(subject, "inc-one", entry);
        claim
            .fields
            .insert("entry_type".into(), Value::String("usage".into()));
        claim
            .fields
            .insert("source_id".into(), Value::String(entry.into()));
        claim
            .fields
            .insert("observed_at_unix_ms".into(), Value::from(at as u64));
        claim.fields.insert(
            "body".into(),
            json!({"semantics": "response", "model": "claude-opus-5-5", "input_tokens": 1, "output_tokens": 1, "total_tokens": 2}),
        );
        store.append_claim_outcome(&claim).unwrap();
    };
    let now = now_ms();
    respond("counted-before", now);
    respond("too-old", now - USAGE_RESPONSE_HORIZON_MS - 60_000);
    respond("recent", now - 60_000);
    let total: u64 = store
        .connection
        .lock()
        .unwrap()
        .query_row(
            "SELECT COALESCE(SUM(total_tokens), 0) FROM local_usage_spend",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(total, 2, "only the recent response counts");
    assert_eq!(store.trim_usage_responses(now - 30_000, 1).unwrap(), 1);
    let remembered = store
        .connection
        .lock()
        .unwrap()
        .prepare("SELECT source_id FROM local_usage_responses ORDER BY source_id")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(remembered, ["counted-before"]);
}

#[test]
fn a_timeline_page_merges_claims_from_older_builds_with_local_observations() {
    let store = Store::open_memory("node").unwrap();
    let subject = "agent/node.worker";
    // An older build wrote timeline entries as replicated claims.
    let legacy = |entry: &str| {
        let input = timeline_observation(subject, "inc-1", entry);
        let mut connection = store.connection.lock().unwrap();
        let transaction = connection.transaction().unwrap();
        append_claim_tx(
            &transaction,
            "older-node",
            subject,
            "harness.timeline",
            Some(subject),
            &json!({ "fields": input.fields, "evidence": [] }),
            &[],
            None,
        )
        .unwrap();
        transaction.commit().unwrap();
    };
    legacy("legacy-1");
    legacy("legacy-2");
    store
        .append_claim(&timeline_observation(subject, "inc-1", "local-1"))
        .unwrap();
    store
        .append_claim(&timeline_observation(subject, "inc-2", "other-incarnation"))
        .unwrap();
    legacy("legacy-3");
    store
        .append_claim(&timeline_observation(subject, "inc-1", "local-2"))
        .unwrap();
    store
        .append_claim(&timeline_observation(subject, "inc-1", "local-3"))
        .unwrap();

    let ascending = store
        .timeline_claims_for_incarnation_at(subject, "inc-1", None, false, 10)
        .unwrap();
    assert_eq!(
        timeline_entries(&ascending),
        [
            "legacy-1", "legacy-2", "local-1", "legacy-3", "local-2", "local-3"
        ]
    );
    let newest = store
        .timeline_claims_for_incarnation_at(subject, "inc-1", None, true, 2)
        .unwrap();
    assert_eq!(timeline_entries(&newest), ["local-3", "local-2"]);
    assert!(newest.next_cursor.is_some());
    let legacy_three = ascending.claims[3].store_index;
    let before = store
        .timeline_claims_for_incarnation_at(subject, "inc-1", Some(legacy_three), true, 10)
        .unwrap();
    assert_eq!(
        timeline_entries(&before),
        ["local-1", "legacy-2", "legacy-1"],
        "a snapshot before a claim excludes the local observations written after it"
    );
    assert_eq!(
        store
            .latest_local_timeline_at(subject, "inc-1", legacy_three)
            .unwrap(),
        Some(ascending.claims[5].accepted_at_unix_ms)
    );
    assert_eq!(
        store
            .latest_local_timeline_at(subject, "inc-1", legacy_three - 1)
            .unwrap(),
        Some(ascending.claims[2].accepted_at_unix_ms)
    );
}

#[test]
fn trimming_local_observations_keeps_the_newest_of_each_subject_and_kind() {
    let store = Store::open_memory("node").unwrap();
    for entry in 0..10 {
        store
            .append_claim(&timeline_observation(
                "agent/node.first",
                "inc",
                &format!("entry-{entry}"),
            ))
            .unwrap();
    }
    for entry in 0..3 {
        store
            .append_claim(&timeline_observation(
                "agent/node.second",
                "inc",
                &format!("entry-{entry}"),
            ))
            .unwrap();
    }
    let positions = |store: &Store| {
        store
            .local_observations_after(0, 100)
            .unwrap()
            .iter()
            .map(|record| local_observation_position(record).unwrap())
            .collect::<Vec<_>>()
    };
    assert_eq!(positions(&store), (1..=13).collect::<Vec<_>>());
    assert_eq!(
        store
            .trim_local_observations(now_ms().saturating_sub(3_600_000), 20_000, 2)
            .unwrap(),
        0,
        "nothing is older than the window yet"
    );
    store
        .connection
        .lock()
        .unwrap()
        .execute(
            "UPDATE local_observations SET observed_at_unix_ms=1000 WHERE id <= 12",
            [],
        )
        .unwrap();
    assert_eq!(
        store.trim_local_observations(2_000, 20_000, 2).unwrap(),
        11,
        "each chunk deletes at most two rows until nothing old is left"
    );
    assert_eq!(
        positions(&store),
        [10, 13],
        "the newest observation of each subject and kind stays"
    );
    assert_eq!(store.local_observations_tail(1).unwrap().len(), 1);

    for entry in 0..5 {
        store
            .append_claim(&timeline_observation(
                "agent/node.third",
                "inc",
                &format!("entry-{entry}"),
            ))
            .unwrap();
    }
    assert_eq!(store.trim_local_observations(0, 2, 100).unwrap(), 3);
    assert_eq!(positions(&store), [10, 13, 17, 18]);
    // AUTOINCREMENT never reuses a trimmed position, so event cursors stay ordered.
    let (record, _) = store
        .append_claim_outcome(&timeline_observation("agent/node.third", "inc", "later"))
        .unwrap();
    assert_eq!(local_observation_position(&record), Some(19));
    assert_eq!(store.local_observation_floor_after_claim(0).unwrap(), 9);
    assert_eq!(
        store
            .local_observation_floor_after_claim(store.index().unwrap() + 1)
            .unwrap(),
        19
    );
}

#[test]
fn local_observation_floor_seeks_the_claim_frontier() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.sqlite3");
    {
        let store = Store::open(&path, "node").unwrap();
        assert_eq!(store.local_observation_floor_after_claim(0).unwrap(), 0);
        let connection = store.connection.write();
        connection
            .execute_batch(
                "INSERT INTO local_observations(id,after_store_index,subject,kind,body,observed_at_unix_ms)
                     VALUES(3,10,'agent/node.a','harness.timeline','{}',1),
                           (7,30,'agent/node.a','harness.timeline','{}',1),
                           (12,20,'agent/node.a','harness.timeline','{}',1);
                     DROP INDEX local_observations_after_store_index;",
            )
            .unwrap();
    }
    // Opening an existing schema adds the index without replaying or changing rows.
    let store = Store::open(&path, "node").unwrap();
    assert_eq!(store.local_observation_floor_after_claim(0).unwrap(), 2);
    assert_eq!(store.local_observation_floor_after_claim(10).unwrap(), 2);
    assert_eq!(store.local_observation_floor_after_claim(11).unwrap(), 6);
    assert_eq!(store.local_observation_floor_after_claim(20).unwrap(), 6);
    assert_eq!(store.local_observation_floor_after_claim(21).unwrap(), 6);
    assert_eq!(store.local_observation_floor_after_claim(31).unwrap(), 12);
    let connection = store.readers.get();
    let plan = connection
        .prepare(&format!("EXPLAIN QUERY PLAN {LOCAL_OBSERVATION_FLOOR_QUERY}"))
        .unwrap()
        .query_map([30], |row| row.get::<_, String>(3))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
        .join("; ");
    assert!(
        plan.contains("USING COVERING INDEX local_observations_after_store_index (after_store_index>?)"),
        "{plan}"
    );
    assert_eq!(
        connection
            .query_row("SELECT COUNT(*) FROM local_observations", [], |row| row.get::<_, u64>(0))
            .unwrap(),
        3
    );
}

#[test]
fn the_per_subject_cap_trims_every_subject_over_it_and_only_those() {
    let store = Store::open_memory("node").unwrap();
    for (subject, entries) in [("agent/node.a", 7), ("agent/node.b", 5), ("agent/node.c", 2)] {
        for entry in 0..entries {
            store
                .append_claim(&timeline_observation(subject, "inc", &format!("entry-{entry}")))
                .unwrap();
        }
    }
    let remaining = |store: &Store| {
        store
            .local_observations_after(0, 100)
            .unwrap()
            .iter()
            .map(|record| (record.subject.clone(), local_observation_position(record).unwrap()))
            .collect::<Vec<_>>()
    };
    // Nothing is old; the cap alone applies, two rows to a chunk, to a and b but not c.
    assert_eq!(store.trim_local_observations(0, 3, 2).unwrap(), 4 + 2);
    let kept = remaining(&store);
    let of = |name: &str| {
        kept.iter()
            .filter(|(subject, _)| subject == name)
            .map(|(_, position)| *position)
            .collect::<Vec<_>>()
    };
    assert_eq!(of("agent/node.a"), [5, 6, 7], "the three newest of seven");
    assert_eq!(of("agent/node.b"), [10, 11, 12], "the three newest of five");
    assert_eq!(of("agent/node.c"), [13, 14], "a subject under the cap is untouched");
    assert_eq!(store.trim_local_observations(0, 3, 2).unwrap(), 0);
}

#[test]
fn a_planning_rebuild_leaves_the_operations_alone() {
    let store = Store::open_memory("alder").unwrap();
    for index in 0..5 {
        store
            .append_claim(&ClaimInput {
                subject: format!("custom/pin/{index}"),
                kind: "custom.drift.note".into(),
                actor: Some("person/tester".into()),
                fields: BTreeMap::new(),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some(format!("pin-{index}")),
            })
            .unwrap();
    }
    let count = |store: &Store| -> i64 {
        store
            .readers
            .get()
            .query_row("SELECT COUNT(*) FROM operations", [], |row| row.get(0))
            .unwrap()
    };
    let rows = count(&store);
    assert!(rows >= 5, "each claim with an idempotency key registers an operation");
    store
        .connection
        .lock()
        .unwrap()
        .execute("DELETE FROM operations", [])
        .unwrap();
    // A planning write rebuilds the planning tables only: it must not re-derive operations
    // (241,000 rows, 19 s on a real store).
    store.rebuild_planning_projection().unwrap();
    assert_eq!(count(&store), 0);
    // The full rebuild does, which is what the planning path used to run on every write.
    store.rebuild_claim_projections().unwrap();
    assert_eq!(count(&store), rows);
}

fn harness_state(subject: &str, state: &str, observed_at_ms: u64) -> ClaimInput {
    ClaimInput {
        subject: subject.into(),
        kind: "harness.observed".into(),
        actor: Some(subject.into()),
        fields: BTreeMap::from([
            ("state".into(), Value::String(state.into())),
            ("incarnation_id".into(), Value::String("inc-1".into())),
            ("observed_at_ms".into(), Value::from(observed_at_ms)),
        ]),
        evidence: Vec::new(),
        expected_subject: None,
        idempotency_key: Some(format!("state:{subject}:{state}:{observed_at_ms}")),
    }
}

fn harness_usage(subject: &str, semantics: &str, tokens: u64, compactions: u64) -> ClaimInput {
    ClaimInput {
        subject: subject.into(),
        kind: "harness.usage".into(),
        actor: Some(subject.into()),
        fields: BTreeMap::from([
            ("driver".into(), Value::String("codex".into())),
            ("incarnation_id".into(), Value::String("inc-1".into())),
            ("semantics".into(), Value::String(semantics.into())),
            ("context_used_tokens".into(), Value::from(tokens)),
            ("compactions".into(), Value::from(compactions)),
            ("model".into(), Value::String("example-model".into())),
        ]),
        evidence: Vec::new(),
        expected_subject: None,
        idempotency_key: Some(format!(
            "usage:{subject}:{semantics}:{tokens}:{compactions}"
        )),
    }
}

#[test]
fn a_harness_state_replicates_only_when_it_changes() {
    let store = Store::open_memory("node").unwrap();
    let subject = "agent/node.worker";
    let inventory_before = store.replication_inventory().unwrap().envelopes.len();
    let mut replicated = Vec::new();
    for (state, observed_at, expect_claim) in [
        ("working", 1_000, true),
        ("working", 301_000, false),
        ("working", 601_000, false),
        ("idle", 602_000, true),
        ("idle", 902_000, false),
        ("working", 903_000, true),
    ] {
        let (record, appended) = store
            .append_claim_outcome(&harness_state(subject, state, observed_at))
            .unwrap();
        assert!(appended);
        assert_eq!(
            local_observation_position(&record).is_none(),
            expect_claim,
            "{state} at {observed_at}"
        );
        if expect_claim {
            replicated.push(record.id);
        }
    }
    let (repeated, appended) = store
        .append_claim_outcome(&harness_state(subject, "idle", 902_000))
        .unwrap();
    assert!(!appended);
    assert!(local_observation_position(&repeated).is_some());

    let claims = store.claims_for(subject, Some("harness.observed")).unwrap();
    assert_eq!(
        claims
            .iter()
            .map(|claim| claim.id.clone())
            .collect::<Vec<_>>(),
        replicated
    );
    assert_eq!(
        store.replication_inventory().unwrap().envelopes.len(),
        inventory_before + 3
    );
    let local = store.local_observations_after(0, 100).unwrap();
    assert_eq!(local.len(), 6, "the local log keeps every observation");
    assert!(local.iter().all(|record| record.kind == "harness.observed"));
    let latest = store
        .latest_claim(subject, Some("harness.observed"))
        .unwrap()
        .unwrap();
    assert_eq!(latest.body["fields"]["state"], "working");
    assert_eq!(latest.body["fields"]["observed_at_ms"], 903_000);

    let replica = Store::open_memory("replica").unwrap();
    receive_and_project(
        &replica,
        "node",
        &exchange_from(&store, &ReplicationInventory::default()),
    );
    assert_eq!(
        replica
            .claims_for(subject, Some("harness.observed"))
            .unwrap()
            .len(),
        3
    );
    assert!(replica.local_observations_after(0, 100).unwrap().is_empty());
}

#[test]
fn harness_usage_replicates_on_a_schedule_while_working_and_at_once_when_it_stops() {
    let store = Store::open_memory("node").unwrap();
    let subject = "agent/node.worker";
    let start = now_ms();
    let minute = 60_000_u128;
    let usage_claims = |store: &Store| {
        store
            .claims_for(subject, Some("harness.usage"))
            .unwrap()
            .iter()
            .map(|claim| {
                (
                    claim.body["fields"]["semantics"]
                        .as_str()
                        .unwrap()
                        .to_owned(),
                    claim.body["fields"]["context_used_tokens"]
                        .as_u64()
                        .unwrap(),
                )
            })
            .collect::<Vec<_>>()
    };
    let occupancy = |tokens: u64| ("context_occupancy".to_owned(), tokens);
    store
        .append_claim_outcome(&harness_state(subject, "working", 1))
        .unwrap();
    let usage = |semantics: &str, tokens: u64, compactions: u64, at: u128| {
        store
            .append_latest_observation(
                &harness_usage(subject, semantics, tokens, compactions),
                at,
            )
            .unwrap()
    };

    assert!(local_observation_position(&usage("context_occupancy", 10, 0, start).0).is_none());
    assert!(
        local_observation_position(&usage("context_occupancy", 20, 0, start + minute).0)
            .is_some()
    );
    assert!(
        local_observation_position(&usage("context_occupancy", 30, 0, start + 2 * minute).0)
            .is_some()
    );
    assert_eq!(
        usage_claims(&store),
        [occupancy(10)],
        "while working, a changed reading waits for the interval"
    );
    assert!(
        local_observation_position(&usage("session_cumulative", 500, 0, start + 2 * minute).0)
            .is_none(),
        "each semantics has its own slot"
    );
    assert!(
        local_observation_position(&usage("context_occupancy", 5, 1, start + 3 * minute).0)
            .is_none(),
        "a compaction replicates at once"
    );
    assert!(
        local_observation_position(&usage("context_occupancy", 15, 1, start + 4 * minute).0)
            .is_some()
    );
    assert!(
        local_observation_position(&usage("context_occupancy", 25, 1, start + 9 * minute).0)
            .is_none(),
        "the interval has passed since the last replicated reading"
    );
    assert!(
        local_observation_position(&usage("context_occupancy", 35, 1, start + 10 * minute).0)
            .is_some()
    );
    assert_eq!(
        usage_claims(&store),
        [
            occupancy(10),
            ("session_cumulative".into(), 500),
            occupancy(5),
            occupancy(25),
        ]
    );

    store
        .append_claim_outcome(&harness_state(subject, "idle", 2))
        .unwrap();
    assert_eq!(
        usage_claims(&store).last(),
        Some(&occupancy(35)),
        "the newest pending reading replicates when the harness stops working"
    );
    let settled = usage_claims(&store).len();
    store
        .append_claim_outcome(&harness_state(subject, "idle", 3))
        .unwrap();
    assert_eq!(usage_claims(&store).len(), settled, "nothing was pending");
    assert!(
        local_observation_position(&usage("context_occupancy", 36, 1, start + 11 * minute).0)
            .is_none(),
        "an idle harness replicates its usage at once"
    );
    assert_eq!(store.local_observations_after(0, 100).unwrap().len(), 12);
}

#[test]
fn a_system_runtime_action_stays_local_while_a_requested_one_replicates() {
    let store = Store::open_memory("node").unwrap();
    let subject = "agent/node.worker";
    let action =
        |action: &str, actor: Option<&str>, evidence: Vec<String>, key: &str| ClaimInput {
            subject: subject.into(),
            kind: if action == "signal-result" {
                "runtime.action.succeeded".into()
            } else {
                "runtime.action.requested".into()
            },
            actor: actor.map(str::to_owned),
            fields: BTreeMap::from([(
                "action".into(),
                Value::String(
                    if action == "signal-result" {
                        "signal"
                    } else {
                        action
                    }
                    .into(),
                ),
            )]),
            evidence,
            expected_subject: None,
            idempotency_key: Some(key.into()),
        };
    let system = store
        .append_claim(&action("terminate", None, Vec::new(), "system-stop"))
        .unwrap();
    assert!(local_observation_position(&system).is_some());
    let deadline = store
        .append_claim(&ClaimInput {
            subject: subject.into(),
            kind: "runtime.action.deadline-reached".into(),
            actor: None,
            fields: BTreeMap::from([("action".into(), Value::String("terminate".into()))]),
            evidence: vec![system.id.clone()],
            expected_subject: None,
            idempotency_key: Some("system-deadline".into()),
        })
        .unwrap();
    assert!(
        local_observation_position(&deadline).is_some(),
        "a local observation may cite another"
    );

    let requested = store
        .append_claim(&action(
            "signal",
            Some("requester"),
            Vec::new(),
            "person-signal",
        ))
        .unwrap();
    assert!(
        local_observation_position(&requested).is_none(),
        "a request that names its actor replicates"
    );
    let result = store
        .append_claim(&action(
            "signal-result",
            Some("requester"),
            vec![requested.id.clone()],
            "person-signal-result",
        ))
        .unwrap();
    assert!(local_observation_position(&result).is_none());
    assert_eq!(
        store
            .append_claim(&action(
                "signal-result",
                Some("requester"),
                vec![system.id.clone()],
                "cites-local",
            ))
            .unwrap_err()
            .code,
        "missing-evidence",
        "a replicated claim never cites a local observation"
    );
    assert_eq!(
        store
            .append_claim(&ClaimInput {
                subject: subject.into(),
                kind: "runtime.action.deadline-reached".into(),
                actor: None,
                fields: BTreeMap::from([("action".into(), Value::String("terminate".into()))]),
                evidence: vec!["local-observation/another-node/1".into()],
                expected_subject: None,
                idempotency_key: Some("foreign-local".into()),
            })
            .unwrap_err()
            .code,
        "missing-evidence",
        "another node's local observation is never here"
    );
    let recorded = store
        .observations_for(subject, "runtime.action.requested")
        .unwrap();
    assert_eq!(
        recorded
            .iter()
            .map(|record| record.id.clone())
            .collect::<Vec<_>>(),
        [system.id, requested.id]
    );
}

#[test]
fn a_retry_records_its_backoff_boundary() {
    let store = Store::open_memory("node").unwrap();
    let publish = |source: &str, key: &str| {
        let intent = crate::graph::parse_test_intent(source, "node").unwrap();
        let planned = store
            .mission(
                &intent,
                IntentInput {
                    kdl: source.into(),
                    source_name: None,
                },
            )
            .unwrap();
        store.apply(&intent, &planned.subject_tokens, key).unwrap();
    };
    publish(
        r#"version 2
 mission "retry" state="ready" { goal "Run retry work."; step "work" { } } "#,
        "retry-mission",
    );
    let run = store
        .create_mission_run(&MissionRunRequest {
            mission: "retry".into(),
            revision: None,
            workspace: "/tmp".into(),
            requester: Some("person/test".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "retry-run".into(),
        })
        .unwrap();
    let subject = &run.steps[0].subject;
    store
        .set_step_state(subject, "failed", Some("first attempt failed"))
        .unwrap();
    store.retry_step(subject, "retry policy", 60_000).unwrap();
    let view = store.step_run(subject).unwrap().unwrap();
    assert_eq!(view.status, "pending");
    assert_eq!(view.attempt, 2);
    assert!(view.not_before_unix_ms.unwrap() >= view.updated_at_unix_ms + 59_000);
}

#[test]
fn a_worker_can_submit_each_retry_attempt_once() {
    let store = Store::open_memory("node").unwrap();
    let source = r#"version 2

agent "worker" { workspace "/tmp"; command "true" }

mission "retry" state="ready" {
  goal "Retry failed work."
  step "work" {
    assigned-to "agent/worker"
    retry { attempts 3 }
    gate "the result is ready" { exists "resource/result" }
  }
}
"#;
    let intent = crate::graph::parse_test_intent(source, "node").unwrap();
    let planned = store
        .mission(
            &intent,
            IntentInput {
                kdl: source.into(),
                source_name: None,
            },
        )
        .unwrap();
    store
        .apply(&intent, &planned.subject_tokens, "loop-mission")
        .unwrap();
    let run = store
        .create_mission_run(&MissionRunRequest {
            mission: "retry".into(),
            revision: None,
            workspace: "/tmp".into(),
            requester: Some("person/test".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "loop-run".into(),
        })
        .unwrap();
    let subject = &run.steps[0].subject;
    let request = |key: &str| WorkRequest {
        wait_ms: None,
        actor: Some("agent/node.worker".into()),
        incarnation: Some("current".into()),
        summary: None,
        reason: None,
        evidence: Vec::new(),
        idempotency_key: key.into(),
    };

    store.set_step_state(subject, "ready", None).unwrap();
    store
        .work_action(subject, "claim", &request("claim-round-1"))
        .unwrap();
    store
        .work_action(subject, "complete", &request("complete-round-1"))
        .unwrap();
    store
        .set_step_state(subject, "failed", Some("the gate failed"))
        .unwrap();
    store.retry_step(subject, "retry attempt 2", 0).unwrap();
    store.set_step_state(subject, "ready", None).unwrap();
    store
        .work_action(subject, "claim", &request("claim-round-2"))
        .unwrap();
    let completed = store
        .work_action(subject, "complete", &request("complete-round-2"))
        .unwrap();
    assert_eq!(completed.status, "verifying");
    assert_eq!(completed.attempt, 2);

    let connection = store.connection.lock().unwrap();
    let mut statement = connection
        .prepare(
            "SELECT CAST(json_extract(body, '$.fields.attempt') AS INTEGER)
                 FROM claims WHERE subject=?1 AND kind='work.submitted' ORDER BY store_index",
        )
        .unwrap();
    let attempts = statement
        .query_map([subject], |row| row.get::<_, u64>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(attempts, [1, 2]);
}

const TAKEOVER_SOURCE: &str = r#"version 2

mission "takeover" state="ready" {
  goal "Take over one service."
  step "prepare" { }
  step "deploy-check" { depends-on { step "prepare" completed } }
  step "smoke-check" { depends-on { step "prepare" completed } }
  step "announce" { depends-on { step "deploy-check" completed } }
  finally { step "report" { agentless } }
}
"#;

fn publish_takeover(store: &Store, source: &str, key: &str) -> MissionSpec {
    let intent = parse_intent(source, "node").unwrap();
    let planned = store
        .mission(
            &intent,
            IntentInput {
                kdl: source.into(),
                source_name: None,
            },
        )
        .unwrap();
    store.apply(&intent, &planned.subject_tokens, key).unwrap();
    intent.missions["takeover"].clone()
}

fn takeover_step<'a>(run: &'a MissionRunView, path: &str) -> &'a StepRunView {
    run.steps.iter().find(|step| step.step == path).unwrap()
}

/// Fail the takeover run the way the reconciler does: the failed steps cancel the rest of the
/// normal work, the final step runs, and cleanup makes the run terminal.
fn failed_takeover_run(store: &Store, failed: &[&str]) -> MissionRunView {
    publish_takeover(store, TAKEOVER_SOURCE, "takeover-mission");
    let run = store
        .create_mission_run(&MissionRunRequest {
            mission: "takeover".into(),
            revision: None,
            workspace: "/tmp".into(),
            requester: Some("person/requester".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "takeover-run".into(),
        })
        .unwrap();
    store
        .set_step_state(&takeover_step(&run, "prepare").subject, "completed", None)
        .unwrap();
    for path in failed {
        store
            .set_step_state(
                &takeover_step(&run, path).subject,
                "failed",
                Some("the check failed"),
            )
            .unwrap();
    }
    for path in ["deploy-check", "smoke-check", "announce"] {
        if !failed.contains(&path) {
            store
                .set_step_state(
                    &takeover_step(&run, path).subject,
                    "cancelled",
                    Some("another step failed"),
                )
                .unwrap();
        }
    }
    store
        .set_mission_run_state(&run.id, "running", "final", Some("a normal step failed"))
        .unwrap();
    store
        .set_step_state(&takeover_step(&run, "report").subject, "completed", None)
        .unwrap();
    store
        .set_mission_run_state(
            &run.id,
            "running",
            "cleanup-failed",
            Some("one or more mission steps failed"),
        )
        .unwrap();
    store
        .set_mission_run_state(&run.id, "failed", "terminal", None)
        .unwrap();
    let run = store.mission_run(&run.id).unwrap().unwrap();
    assert_eq!(
        (run.status.as_str(), run.phase.as_str()),
        ("failed", "terminal")
    );
    run
}

#[test]
fn retention_event_tail_work_is_bounded_during_and_after_migration() {
    let connection = rusqlite::Connection::open_in_memory().unwrap();
    connection
        .execute_batch(
            "CREATE TABLE claims(store_index INTEGER PRIMARY KEY,kind TEXT,body TEXT);
             CREATE TABLE event_positions(store_index INTEGER PRIMARY KEY,subject TEXT);
             CREATE TABLE local_event_payloads(store_index INTEGER PRIMARY KEY,subject TEXT);",
        )
        .unwrap();
    for count in [1_000, 10_000, 100_000] {
        connection.execute_batch(&format!(
            "DELETE FROM claims; DELETE FROM event_positions; DELETE FROM local_event_payloads;
                 WITH RECURSIVE numbers(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM numbers WHERE n<{count})
                 INSERT INTO claims SELECT n,'custom.test.recorded','{{}}' FROM numbers;
                 INSERT INTO event_positions SELECT store_index,'custom/test/tail' FROM claims WHERE store_index%2=0;
                 INSERT INTO local_event_payloads SELECT store_index,'custom/test/tail' FROM claims WHERE store_index%2=1;"
        )).unwrap();
        for migrating in [true, false] {
            let mut statement = connection.prepare(event_tail_sql(migrating)).unwrap();
            let indices = statement
                .query_map([3], |row| row.get::<_, u64>(0))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            let expected = if migrating {
                vec![count - 2, count - 1, count]
            } else {
                vec![count - 4, count - 2, count]
            };
            assert_eq!(indices, expected);
            assert!(
                statement.get_status(rusqlite::StatementStatus::VmStep) < 1_000,
                "{count} rows, migrating={migrating}"
            );
            assert!(
                statement.get_status(rusqlite::StatementStatus::FullscanStep) <= 12,
                "only bounded membership and result steps"
            );
        }
    }
}

#[test]
fn retention_event_publication_bodies_match_the_old_payload_contract() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("claims.sqlite");
    let store = Store::open(&path, "node").unwrap();
    let source = format!(
        "{TAKEOVER_SOURCE}\nagent \"payload-worker\" {{ workspace \"/tmp\"; command \"true\" }}\n"
    );
    let intent = parse_intent(&source, "node").unwrap();
    let planned = store
        .mission(
            &intent,
            IntentInput {
                kdl: source,
                source_name: None,
            },
        )
        .unwrap();
    store
        .apply(&intent, &planned.subject_tokens, "retention-event-payload")
        .unwrap();
    let admitted = store.events_after(0, None).unwrap();
    let mut old_payloads = Vec::new();
    {
        let connection = store.connection.write();
        // Restore the actual schema-16 payload table. Populate it with the inputs used by
        // the old insert_event, independently of the new join's payload results.
        connection
            .execute_batch(
                "DROP VIEW events; DROP TABLE event_positions;
                 CREATE TABLE events (
                     store_index INTEGER PRIMARY KEY, kind TEXT NOT NULL,
                     subject TEXT NOT NULL, body TEXT NOT NULL
                 ); PRAGMA user_version=16;",
            )
            .unwrap();
        for (subject, kind, mut old_body) in [
            (
                "agent/node.payload-worker",
                "intent.desired",
                serde_json::to_value(&intent.subjects["agent/node.payload-worker"]).unwrap(),
            ),
            (
                "mission/takeover",
                "mission.published",
                serde_json::to_value(&intent.missions["takeover"]).unwrap(),
            ),
        ] {
            let event = admitted
                .iter()
                .find(|event| event.subject == subject && event.kind == kind)
                .unwrap();
            if let Some(operation) = event.body.get("_operation") {
                old_body["_operation"] = operation.clone();
            }
            // This is schema-16 insert_event's INSERT and canonicalization, rather than
            // an INSERT SELECT from claims or the new view.
            connection.execute(
                "INSERT OR IGNORE INTO events(store_index, kind, subject, body) VALUES (?1, ?2, ?3, ?4)",
                params![event.store_index, kind, subject, canonical_json_text(&old_body).unwrap()],
            ).unwrap();
        }
        let mut statement = connection
            .prepare("SELECT store_index,kind,subject,body FROM events ORDER BY store_index")
            .unwrap();
        old_payloads.extend(
            statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, u64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                })
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap(),
        );
        assert_eq!(old_payloads.len(), 2);
    }
    drop(store);
    let store = Store::open(&path, "node").unwrap();
    let read_payloads = || {
        let connection = store.readers.get();
        let mut statement = connection
            .prepare("SELECT store_index,kind,subject,body FROM events ORDER BY store_index")
            .unwrap();
        statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, u64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    };
    assert!(store.event_payload_migration_pending().unwrap());
    assert_eq!(
        read_payloads(),
        old_payloads,
        "upgrade view must match old stored bytes"
    );
    assert_eq!(store.migrate_event_payloads().unwrap(), 2);
    assert!(!store.event_payload_migration_pending().unwrap());
    assert_eq!(
        read_payloads(),
        old_payloads,
        "completed payload-free view must match old stored bytes"
    );
}

fn retention_failed_active_step(store: &Store) -> (MissionRunView, String) {
    publish_takeover(store, TAKEOVER_SOURCE, "retention-mission");
    let run = store
        .create_mission_run(&MissionRunRequest {
            mission: "takeover".into(),
            revision: None,
            workspace: "/tmp".into(),
            requester: Some("person/requester".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "retention-run".into(),
        })
        .unwrap();
    let subject = takeover_step(&run, "deploy-check").subject.clone();
    store
        .set_step_state(&subject, "failed", Some("fixture failure"))
        .unwrap();
    (run, subject)
}

#[test]
fn retention_expired_step_retry_refuses_before_any_second_effect() {
    let store = Store::open_memory("node").unwrap();
    let (run, subject) = retention_failed_active_step(&store);
    store
        .retry_failed_step(&subject, "person/operator", "retry", "expired-retry")
        .unwrap();
    // Finish the generation, so the active-response pin no longer postpones expiry.
    store
        .set_mission_run_state(&run.id, "failed", "terminal", None)
        .unwrap();
    store
        .connection
        .write()
        .execute(
            "DELETE FROM idempotency WHERE operation_id<>?1",
            [opaque_cache_key("expired-retry")],
        )
        .unwrap();
    store
        .connection
        .write()
        .execute("UPDATE idempotency SET expires_at_unix_ms=0", [])
        .unwrap();
    assert_eq!(store.cleanup_idempotency(0).unwrap().deleted, 1);
    let before = store.index().unwrap();
    for _ in 0..2 {
        let error = store
            .retry_failed_step(&subject, "person/operator", "retry", "expired-retry")
            .unwrap_err();
        assert_eq!(error.code, "idempotency-key-expired");
        assert!(error.details.get("claim_id").is_some());
    }
    assert_eq!(store.index().unwrap(), before);
    assert_eq!(store.step_run(&subject).unwrap().unwrap().attempt, 2);
}

#[test]
fn retention_expired_quiet_renewal_cannot_extend_a_released_lease() {
    let (_controller, worker, step) = replicated_step_pair();
    worker_work(&worker, &step, "claim", None, "renew-claim");
    let before = worker.index().unwrap();
    worker_work(&worker, &step, "renew", None, "expired-quiet-renew");
    assert_eq!(
        worker.index().unwrap(),
        before,
        "quiet renewals stay claimless"
    );
    worker_work(&worker, &step, "release", None, "renew-release");
    let released = worker.step_run(&step).unwrap().unwrap();
    worker
        .set_mission_run_state(&released.run, "cancelled", "terminal", None)
        .unwrap();
    worker
        .connection
        .write()
        .execute(
            "DELETE FROM idempotency WHERE operation_id<>?1",
            [opaque_cache_key("expired-quiet-renew")],
        )
        .unwrap();
    worker
        .connection
        .write()
        .execute("UPDATE idempotency SET expires_at_unix_ms=0", [])
        .unwrap();
    assert_eq!(worker.cleanup_idempotency(0).unwrap().deleted, 1);
    let before = worker.index().unwrap();
    let error = worker
        .work_action(
            &step,
            "renew",
            &WorkRequest {
                wait_ms: None,
                actor: Some("agent/worker.one".into()),
                incarnation: Some("worker-generation".into()),
                summary: None,
                reason: None,
                evidence: Vec::new(),
                idempotency_key: "expired-quiet-renew".into(),
            },
        )
        .unwrap_err();
    assert_ne!(
        error.code, "idempotency-key-expired",
        "the normal ownership/state fence decides a claimless retry"
    );
    assert_eq!(worker.index().unwrap(), before);
    let after = worker.step_run(&step).unwrap().unwrap();
    assert_eq!(after.claimant, None);
    assert_eq!(after.claim_expires_at_unix_ms, None);
}

#[test]
fn retention_expired_compound_apply_uses_existing_operation_identities() {
    let store = Store::open_memory("node").unwrap();
    let declaration = crate::graph::parse_execution_intent(r#"version 2
resource "receipt/file" { kind "filesystem.file" }
observer "receipt/file" { resource "resource/receipt/file"; provider "local.file"; locator "/tmp/receipt-file"; field "status" }
"#,"node","receipt-run").unwrap();
    store
        .apply_internal(&declaration, "receipt-declaration")
        .unwrap();
    let source = r#"version 2
resource "receipt/file" {
    refresh "first" { timeout "1s" }
    refresh "second" { timeout "1s" }
}
"#;
    let intent = crate::graph::parse_intent(source, "node").unwrap();
    let planned = store
        .mission(
            &intent,
            IntentInput {
                kdl: source.into(),
                source_name: None,
            },
        )
        .unwrap();
    let first = store
        .apply_as(
            &intent,
            &planned.subject_tokens,
            "expired-compound",
            Some("person/operator"),
        )
        .unwrap();
    assert!(first.changed);
    let operations = store
        .connection
        .write()
        .query_row("SELECT COUNT(*) FROM operations", [], |row| {
            row.get::<_, u64>(0)
        })
        .unwrap();
    store
        .connection
        .write()
        .execute(
            "DELETE FROM idempotency WHERE operation_id<>?1",
            [opaque_cache_key("expired-compound")],
        )
        .unwrap();
    store
        .connection
        .write()
        .execute("UPDATE idempotency SET expires_at_unix_ms=0", [])
        .unwrap();
    assert_eq!(store.cleanup_idempotency(0).unwrap().deleted, 1);
    let before = store.index().unwrap();
    let current = store
        .mission(
            &intent,
            IntentInput {
                kdl: source.into(),
                source_name: None,
            },
        )
        .unwrap();
    let repeat = store
        .apply_as(
            &intent,
            &current.subject_tokens,
            "expired-compound",
            Some("person/operator"),
        )
        .unwrap();
    assert!(!repeat.changed);
    assert_eq!(store.index().unwrap(), before);
    assert_eq!(
        store
            .connection
            .write()
            .query_row("SELECT COUNT(*) FROM operations", [], |row| row
                .get::<_, u64>(0))
            .unwrap(),
        operations
    );
}

#[test]
fn retention_retry_response_failure_rolls_back_the_transition() {
    let store = Store::open_memory("node").unwrap();
    let (_, subject) = retention_failed_active_step(&store);
    let before = store.index().unwrap();
    store
        .connection
        .write()
        .execute_batch(
            "CREATE TRIGGER retention_receipt_failure BEFORE INSERT ON idempotency
             BEGIN SELECT RAISE(ABORT,'injected lost receipt'); END;",
        )
        .unwrap();
    assert!(
        store
            .retry_failed_step(&subject, "person/operator", "retry", "retention-retry")
            .is_err()
    );
    let unchanged = store.step_run(&subject).unwrap().unwrap();
    assert_eq!(
        (unchanged.status.as_str(), unchanged.attempt),
        ("failed", 1)
    );
    assert_eq!(store.index().unwrap(), before);
    store
        .connection
        .write()
        .execute_batch("DROP TRIGGER retention_receipt_failure")
        .unwrap();
    let result = store
        .retry_failed_step(&subject, "person/operator", "retry", "retention-retry")
        .unwrap();
    assert_eq!(takeover_step(&result, "deploy-check").attempt, 2);
}

#[test]
fn retention_concurrent_retry_commits_once_and_returns_one_response() {
    let store = Arc::new(Store::open_memory("node").unwrap());
    let (_, subject) = retention_failed_active_step(&store);
    let barrier = Arc::new(std::sync::Barrier::new(12));
    let workers = (0..12)
        .map(|_| {
            let store = store.clone();
            let barrier = barrier.clone();
            let subject = subject.clone();
            std::thread::spawn(move || {
                barrier.wait();
                store
                    .retry_failed_step(&subject, "person/operator", "retry", "retention-retry")
                    .unwrap()
            })
        })
        .collect::<Vec<_>>();
    let results = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect::<Vec<_>>();
    let first = serde_json::to_value(&results[0]).unwrap();
    assert!(
        results
            .iter()
            .all(|result| serde_json::to_value(result).unwrap() == first)
    );
    assert_eq!(
        store
            .claims_for(&subject, Some("step-run.retried"))
            .unwrap()
            .len(),
        1
    );
    assert_eq!(store.step_run(&subject).unwrap().unwrap().attempt, 2);
}

#[test]
fn retention_retry_survives_restart_and_active_generation_expiry() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("claims.sqlite");
    let store = Store::open(&path, "node").unwrap();
    let (_, subject) = retention_failed_active_step(&store);
    let response = store
        .retry_failed_step(&subject, "person/operator", "retry", "retention-retry")
        .unwrap();
    drop(store);
    let store = Store::open(&path, "node").unwrap();
    let repeated = store
        .retry_failed_step(&subject, "person/operator", "retry", "retention-retry")
        .unwrap();
    assert_eq!(
        serde_json::to_value(&repeated).unwrap(),
        serde_json::to_value(&response).unwrap()
    );
    store
        .connection
        .write()
        .execute(
            "DELETE FROM idempotency WHERE operation_id<>?1",
            [opaque_cache_key("retention-retry")],
        )
        .unwrap();
    store
        .connection
        .write()
        .execute(
            "UPDATE idempotency SET expires_at_unix_ms=0 WHERE operation_id=?1",
            [opaque_cache_key("retention-retry")],
        )
        .unwrap();
    let cleanup = store.cleanup_idempotency(0).unwrap();
    assert_eq!(cleanup.extended, 1);
    assert_eq!(cleanup.deleted, 0);
    assert!(
        store
            .cached_idempotency_response::<MissionRunView>("retention-retry")
            .unwrap()
            .is_some()
    );
    assert_eq!(
        store
            .claims_for(&subject, Some("step-run.retried"))
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn retention_active_response_shapes_remain_pinned_until_the_run_finishes() {
    let store = Store::open_memory("node").unwrap();
    let (run, _) = retention_failed_active_step(&store);
    store
        .connection
        .write()
        .execute("DELETE FROM idempotency", [])
        .unwrap();
    let responses = [
        serde_json::to_value(&run).unwrap(),
        json!({"mission_run": run}),
        json!({"mission_run": run.subject, "generation": run.generation}),
        json!({"mission_run": run.subject}),
        json!({"mission_run": null, "source_generation": run.generation}),
        json!({"subject_tokens": BTreeMap::from([(run.subject.clone(), Vec::<String>::new())])}),
    ];
    let keys = (0..responses.len())
        .map(|index| format!("retention-shape-{index}"))
        .collect::<Vec<_>>();
    for (key, response) in keys.iter().zip(&responses) {
        store.cache_idempotency_response(key, response).unwrap();
        store
            .connection
            .write()
            .execute(
                "UPDATE idempotency SET expires_at_unix_ms=0 WHERE operation_id=?1",
                [opaque_cache_key(key)],
            )
            .unwrap();
    }
    let cleanup = store.cleanup_idempotency(0).unwrap();
    assert_eq!(cleanup.extended, responses.len());
    assert_eq!(cleanup.deleted, 0);
    for (key, response) in keys.iter().zip(&responses) {
        assert_eq!(
            store
                .cached_idempotency_response::<Value>(key)
                .unwrap()
                .as_ref(),
            Some(response)
        );
    }
    store
        .set_mission_run_state(&run.id, "failed", "terminal", Some("fixture finished"))
        .unwrap();
    for key in &keys {
        store
            .connection
            .write()
            .execute(
                "UPDATE idempotency SET expires_at_unix_ms=0 WHERE operation_id=?1",
                [opaque_cache_key(key)],
            )
            .unwrap();
    }
    let cleanup = store.cleanup_idempotency(0).unwrap();
    assert_eq!(cleanup.deleted, responses.len());
    assert_eq!(cleanup.extended, 0);
}

#[test]
fn retrying_the_only_failed_step_reopens_its_failed_run() {
    let store = Store::open_memory("node").unwrap();
    let failed = failed_takeover_run(&store, &["deploy-check"]);
    let old_check = takeover_step(&failed, "deploy-check").subject.clone();

    let reopened = store
        .retry_failed_step(
            &old_check,
            "person/operator",
            "the deploy check host is back",
            "retry-deploy-check",
        )
        .unwrap();

    assert_eq!(
        (reopened.status.as_str(), reopened.phase.as_str()),
        ("running", "normal")
    );
    assert_ne!(reopened.generation, failed.generation);
    assert_eq!(reopened.revision, failed.revision);
    assert!(reopened.outcome.is_none());
    // The run's state history says who reopened it and why, and dates its state from then.
    let reopening = store
        .latest_claim(&failed.subject, Some("mission-run.state"))
        .unwrap()
        .unwrap();
    assert_eq!(reopening.actor.as_deref(), Some("person/operator"));
    assert_eq!(
        reopening.body["fields"],
        json!({
            "status": "running",
            "phase": "normal",
            "previous_phase": "terminal",
            "reason": "the deploy check host is back",
        })
    );
    let moment = &store.mission_run_states().unwrap()[&failed.subject];
    assert_eq!(moment.since_unix_ms, reopening.accepted_at_unix_ms);
    assert!(moment.outcome.is_none());
    let state = |path| {
        let step = takeover_step(&reopened, path);
        (step.status.as_str(), step.attempt)
    };
    assert_eq!(state("prepare"), ("completed", 1));
    assert_eq!(state("deploy-check"), ("pending", 2));
    assert_eq!(state("smoke-check"), ("pending", 1));
    assert_eq!(state("announce"), ("pending", 1));
    assert_eq!(state("report"), ("pending", 1));
    let generations = store.run_generations(&failed.id).unwrap();
    assert_eq!(
        generations
            .iter()
            .find(|generation| generation.subject == failed.generation)
            .unwrap()
            .status,
        "superseded"
    );

    let repeated = store
        .retry_failed_step(
            &old_check,
            "person/operator",
            "the deploy check host is back",
            "retry-deploy-check",
        )
        .unwrap();
    assert_eq!(repeated.generation, reopened.generation);
    let stale = store
        .retry_failed_step(&old_check, "person/operator", "again", "retry-stale")
        .unwrap_err();
    assert_eq!(stale.code, "stale-run-generation");
}

#[test]
fn a_failed_mission_stays_in_the_current_view_for_a_day() {
    let store = Store::open_memory("node").unwrap();
    let failed = failed_takeover_run(&store, &["deploy-check"]);
    assert_eq!(failed.status, "failed");
    let current = store.mission_collection_ids(false, 0, 50).unwrap();
    assert!(current.contains(&failed.mission), "{current:?}");

    // A day later it is history only.
    let old = crate::store::recently_ended_since().saturating_sub(1_000);
    store
        .connection
        .lock()
        .unwrap()
        .execute(
            "UPDATE mission_runs SET updated_at_unix_ms=?2 WHERE id=?1",
            params![failed.id, old.to_string()],
        )
        .unwrap();
    let current = store.mission_collection_ids(false, 0, 50).unwrap();
    assert!(!current.contains(&failed.mission), "{current:?}");
    let history = store.mission_collection_ids(true, 0, 50).unwrap();
    assert!(history.contains(&failed.mission), "{history:?}");
}

#[test]
fn a_retired_mission_leaves_the_current_view_at_once_after_a_failed_run() {
    let store = Store::open_memory("node").unwrap();
    let failed = failed_takeover_run(&store, &["deploy-check"]);
    store
        .retire_mission(&failed.mission, "person/operator", "retire-failed")
        .unwrap();
    let current = store.mission_collection_ids(false, 0, 50).unwrap();
    assert!(!current.contains(&failed.mission), "{current:?}");
    let history = store.mission_collection_ids(true, 0, 50).unwrap();
    assert!(history.contains(&failed.mission), "{history:?}");
    assert_eq!(
        store.mission_run(&failed.id).unwrap().unwrap().status,
        "failed",
        "retiring the mission leaves its runs as they ended"
    );
}

#[test]
fn a_failed_run_does_not_reopen_for_one_of_several_failures() {
    let store = Store::open_memory("node").unwrap();
    let failed = failed_takeover_run(&store, &["deploy-check", "smoke-check"]);

    let error = store
        .retry_failed_step(
            &takeover_step(&failed, "deploy-check").subject,
            "person/operator",
            "retry one check",
            "retry-one-of-two",
        )
        .unwrap_err();

    assert_eq!(error.code, "mission-run-not-reopenable");
    assert!(
        error.message.contains("step `smoke-check` also failed"),
        "{}",
        error.message
    );
    let run = store.mission_run(&failed.id).unwrap().unwrap();
    assert_eq!(
        (run.status.as_str(), run.generation.as_str()),
        ("failed", failed.generation.as_str())
    );
}

#[test]
fn a_failed_run_with_work_cancelled_for_another_reason_does_not_reopen() {
    let store = Store::open_memory("node").unwrap();
    publish_takeover(&store, TAKEOVER_SOURCE, "takeover-mission");
    let run = store
        .create_mission_run(&MissionRunRequest {
            mission: "takeover".into(),
            revision: None,
            workspace: "/tmp".into(),
            requester: Some("person/requester".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "takeover-run".into(),
        })
        .unwrap();
    let check = takeover_step(&run, "deploy-check").subject.clone();
    store
        .set_step_state(&check, "failed", Some("the check failed"))
        .unwrap();
    store
        .set_mission_run_state(
            &run.id,
            "running",
            "cleanup-failed",
            Some("the mission timeout expired after 50ms"),
        )
        .unwrap();
    store
        .set_mission_run_state(&run.id, "failed", "terminal", None)
        .unwrap();

    let error = store
        .retry_failed_step(&check, "person/operator", "retry", "retry-timeout")
        .unwrap_err();

    assert_eq!(error.code, "mission-run-not-reopenable");
    assert!(
        error
            .message
            .contains("was cancelled: the mission timeout expired"),
        "{}",
        error.message
    );
}

#[test]
fn an_active_run_retries_a_failed_step_in_place() {
    let store = Store::open_memory("node").unwrap();
    publish_takeover(&store, TAKEOVER_SOURCE, "takeover-mission");
    let run = store
        .create_mission_run(&MissionRunRequest {
            mission: "takeover".into(),
            revision: None,
            workspace: "/tmp".into(),
            requester: Some("person/requester".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "takeover-run".into(),
        })
        .unwrap();
    let check = takeover_step(&run, "deploy-check").subject.clone();
    store
        .set_step_state(&check, "failed", Some("the check failed"))
        .unwrap();

    let retried = store
        .retry_failed_step(
            &check,
            "person/operator",
            "retry the check",
            "retry-in-place",
        )
        .unwrap();

    assert_eq!(retried.generation, run.generation);
    let step = takeover_step(&retried, "deploy-check");
    assert_eq!((step.status.as_str(), step.attempt), ("pending", 2));
    let claim = store
        .latest_claim(&check, Some("step-run.retried"))
        .unwrap()
        .unwrap();
    assert_eq!(claim.actor.as_deref(), Some("person/operator"));
    let error = store
        .retry_failed_step(&check, "person/operator", "again", "retry-pending")
        .unwrap_err();
    assert_eq!(error.code, "work-not-failed");
}

#[test]
fn revising_a_failed_run_reopens_it_and_retries_each_failed_step() {
    let store = Store::open_memory("node").unwrap();
    let failed = failed_takeover_run(&store, &["deploy-check", "smoke-check"]);
    let revised = publish_takeover(
        &store,
        &TAKEOVER_SOURCE.replace(
            r#"step "smoke-check" { depends-on"#,
            r#"step "smoke-check" { goal "Probe the new health endpoint."; depends-on"#,
        ),
        "takeover-revision",
    );

    let reopened = store
        .adopt_mission_revision(
            &failed.id,
            &revised,
            "person/requester",
            "the smoke check probes the new endpoint",
            "revise-failed-takeover",
        )
        .unwrap();

    assert_eq!(
        (reopened.status.as_str(), reopened.phase.as_str()),
        ("running", "normal")
    );
    assert_eq!(reopened.revision, revised.revision);
    let state = |path| {
        let step = takeover_step(&reopened, path);
        (step.status.as_str(), step.attempt)
    };
    assert_eq!(state("prepare"), ("completed", 1));
    assert_eq!(state("deploy-check"), ("pending", 2));
    assert_eq!(state("smoke-check"), ("pending", 1));
    assert_eq!(state("announce"), ("pending", 1));
    assert_eq!(state("report"), ("pending", 1));
}

#[test]
fn replication_carries_a_reopened_failed_run() {
    let source = Store::open_memory("source").unwrap();
    let failed = failed_takeover_run(&source, &["deploy-check"]);
    let target = Store::open_memory("target").unwrap();
    target
        .import_replication("source", &source.export_replication(0).unwrap())
        .unwrap();
    assert_eq!(
        target.mission_run(&failed.id).unwrap().unwrap().status,
        "failed"
    );

    let reopened = source
        .retry_failed_step(
            &takeover_step(&failed, "deploy-check").subject,
            "person/operator",
            "the deploy check host is back",
            "retry-replicated",
        )
        .unwrap();
    target
        .import_replication(
            "source",
            &source
                .export_replication_for_heads(&target.replica_heads().unwrap())
                .unwrap(),
        )
        .unwrap();

    let replicated = target.mission_run(&failed.id).unwrap().unwrap();
    assert_eq!(
        (replicated.status.as_str(), replicated.phase.as_str()),
        ("running", "normal")
    );
    assert_eq!(replicated.generation, reopened.generation);
    let states = |run: &MissionRunView| {
        run.steps
            .iter()
            .map(|step| (step.step.clone(), step.status.clone(), step.attempt))
            .collect::<Vec<_>>()
    };
    assert_eq!(states(&replicated), states(&reopened));
}

/// Every node that holds the same claims shows the same graph, however it projected them:
/// one exchange at a time as the source writes, from envelopes that arrive newest first, or
/// all at once as a node that joins late.
#[test]
fn graph_digests_agree_after_sync_whatever_path_projected_the_graph() {
    let source = Store::open_memory("source").unwrap();
    let stepwise = Store::open_memory("stepwise").unwrap();
    let reversed = Store::open_memory("reversed").unwrap();
    let late = Store::open_memory("late").unwrap();
    let sync = |target: &Store| {
        receive_and_project(
            target,
            "source",
            &exchange_from(&source, &target.replication_inventory().unwrap()),
        );
    };

    let failed = failed_takeover_run(&source, &["deploy-check"]);
    sync(&stepwise);
    sync(&reversed);
    // Revising a failed run reopens it. Each carried step claim is its own batch.
    let revised = publish_takeover(
        &source,
        &TAKEOVER_SOURCE.replace(
            r#"step "smoke-check" { depends-on"#,
            r#"step "smoke-check" { goal "Probe the new health endpoint."; depends-on"#,
        ),
        "takeover-revision",
    );
    let reopened = source
        .adopt_mission_revision(
            &failed.id,
            &revised,
            "person/requester",
            "the smoke check probes the new endpoint",
            "revise-failed-takeover",
        )
        .unwrap();
    sync(&stepwise);
    source
        .set_step_state(
            &takeover_step(&reopened, "deploy-check").subject,
            "ready",
            None,
        )
        .unwrap();
    sync(&stepwise);
    // A node admits the envelopes of one exchange in sequence order, so the newest arrives
    // in an exchange of its own and the older ones follow in the next.
    let newest_first = |target: &Store| {
        let mut older = exchange_from(&source, &target.replication_inventory().unwrap());
        let newest = older.envelopes.split_off(older.envelopes.len() - 1);
        receive_and_project(
            target,
            "source",
            &ReplicationExchange {
                envelopes: newest,
                ..older.clone()
            },
        );
        receive_and_project(target, "source", &older);
    };
    newest_first(&reversed);
    // Later writes that arrive out of order make each node rebuild the run tree it already
    // projected, including the reopened generation, without replaying the graph.
    let smoke = takeover_step(&reopened, "smoke-check").subject.clone();
    source.set_step_state(&smoke, "ready", None).unwrap();
    source
        .set_step_state(&smoke, "working", Some("the smoke check started"))
        .unwrap();
    FULL_REPLAYS.with(|replays| replays.set(0));
    newest_first(&stepwise);
    newest_first(&reversed);
    assert_eq!(FULL_REPLAYS.with(std::cell::Cell::get), 0);
    // A replay from nothing, as a restore runs, projects the same graph.
    stepwise.replay_replication_graph().unwrap();
    reversed.replay_replication_graph().unwrap();
    sync(&late);

    let graph = |store: &Store| {
        let run = store.mission_run(&failed.id).unwrap().unwrap();
        let steps = run
            .steps
            .iter()
            .map(|step| (step.step.clone(), step.status.clone(), step.attempt))
            .collect::<Vec<_>>();
        let digest = store
            .replication_status(true, Some(TEST_FLEET), &[])
            .unwrap()
            .graph_digest;
        (run.status, run.phase, steps, digest)
    };
    let expected = graph(&source);
    assert_eq!(
        (expected.0.as_str(), expected.1.as_str()),
        ("running", "normal")
    );
    for (name, target) in [
        ("stepwise", &stepwise),
        ("reversed", &reversed),
        ("late", &late),
    ] {
        assert_eq!(
            canonical_audit::shared_rows(target),
            canonical_audit::shared_rows(&source),
            "{name}: every shared column"
        );
        assert_eq!(graph(target), expected, "{name}");
    }
}

/// A retry that reopened a failed run, and an outcome a person set on a finished run, show
/// the same on a node that replays its claims from nothing, with the times the claims hold.
#[test]
fn a_reopened_run_and_a_set_outcome_survive_a_replay_from_nothing() {
    let sync = |source: &Store, target: &Store| {
        receive_and_project(
            target,
            "source",
            &exchange_from(source, &target.replication_inventory().unwrap()),
        );
    };
    // The newest envelope arrives before the older ones, so the target rebuilds the run tree
    // it reaches out of order; then it replays from nothing, as a restore does.
    let replay = |source: &Store, target: &Store| {
        let mut older = exchange_from(source, &target.replication_inventory().unwrap());
        let newest = older.envelopes.split_off(older.envelopes.len() - 1);
        FULL_REPLAYS.with(|replays| replays.set(0));
        receive_and_project(
            target,
            "source",
            &ReplicationExchange {
                envelopes: newest,
                ..older.clone()
            },
        );
        receive_and_project(target, "source", &older);
        assert_eq!(
            FULL_REPLAYS.with(std::cell::Cell::get),
            0,
            "out-of-order history rebuilds only the run tree it reaches"
        );
        target.replay_replication_graph().unwrap();
    };
    let shown = |store: &Store, run: &MissionRunView| {
        let view = store.mission_run(&run.id).unwrap().unwrap();
        let steps = view
            .steps
            .iter()
            .map(|step| (step.step.clone(), step.status.clone(), step.attempt))
            .collect::<Vec<_>>();
        let since = store.mission_run_states().unwrap()[&run.subject].since_unix_ms;
        (
            view.status,
            view.phase,
            view.generation,
            view.outcome,
            steps,
            since,
        )
    };

    let source = Store::open_memory("source").unwrap();
    let target = Store::open_memory("target").unwrap();
    let failed = failed_takeover_run(&source, &["deploy-check"]);
    sync(&source, &target);
    let reopened = source
        .retry_failed_step(
            &takeover_step(&failed, "deploy-check").subject,
            "person/operator",
            "the deploy check host is back",
            "retry-replayed",
        )
        .unwrap();
    source
        .set_step_state(
            &takeover_step(&reopened, "deploy-check").subject,
            "ready",
            None,
        )
        .unwrap();
    replay(&source, &target);
    let expected = shown(&source, &failed);
    assert_eq!(
        (expected.0.as_str(), expected.1.as_str()),
        ("running", "normal")
    );
    assert_eq!(expected.2, reopened.generation);
    assert_eq!(shown(&target, &failed), expected);

    let source = Store::open_memory("source").unwrap();
    let target = Store::open_memory("target").unwrap();
    let failed = failed_takeover_run(&source, &["deploy-check"]);
    sync(&source, &target);
    for (status, reason, key) in [
        ("completed", "the deploy shipped", "outcome-completed"),
        ("failed", "the deploy was rolled back", "outcome-failed"),
        (
            "completed",
            "the deploy shipped again",
            "outcome-completed-again",
        ),
    ] {
        source
            .set_mission_run_outcome(&failed.subject, status, "person/operator", reason, key)
            .unwrap();
    }
    replay(&source, &target);
    let expected = shown(&source, &failed);
    assert_eq!(
        (expected.0.as_str(), expected.1.as_str()),
        ("completed", "terminal")
    );
    assert_eq!(
        expected.3.as_ref().map(|outcome| outcome.reason.as_str()),
        Some("the deploy shipped again")
    );
    assert_eq!(shown(&target, &failed), expected);
}

/// A node written by a build from before the claim-log diet upgrades in place, keeps
/// writing, and syncs with a node that joins late. Both show the graph the old build showed.
///
/// The fixture is what 91af810a (main before #604) wrote: `failed_takeover_run` with
/// deploy-check failed, `adopt_mission_revision` reopening it with the smoke-check goal
/// changed, deploy-check set ready, then four `harness.observed` heartbeats, one
/// `harness.usage`, one `harness.timeline` and a system `runtime.action.requested` and
/// `runtime.action.succeeded`, all of which replicated before the diet. The store was
/// vacuumed with 512-byte pages and gzipped.
#[test]
fn a_store_written_before_the_log_diet_upgrades_and_syncs_to_the_same_graph() {
    use std::io::Read;

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("claims.sqlite3");
    let mut bytes = Vec::new();
    flate2::read::GzDecoder::new(
        &include_bytes!("../../tests/fixtures/pre-diet-store.sqlite3.gz")[..],
    )
    .read_to_end(&mut bytes)
    .unwrap();
    std::fs::write(&path, bytes).unwrap();

    let upgraded = Store::open(&path, "writer").unwrap();
    upgraded.bind_fleet(TEST_FLEET).unwrap();
    // As the daemon starts. The old build showed the reopened run running, so nothing settles.
    upgraded.validate_replication_backlog().unwrap();
    upgraded.apply_replication_repairs().unwrap();
    assert!(
        upgraded
            .settle_runs_for_canonical_replay()
            .unwrap()
            .is_empty()
    );
    assert!(upgraded.project_replication_backlog().unwrap());
    // After the upgrade, a system runtime action stays local and a heartbeat that changes
    // nothing but its time does not replicate.
    let agent = "agent/writer.worker";
    for (kind, actor, fields) in [
        (
            "runtime.action.requested",
            None,
            json!({"action": "terminate"}),
        ),
        (
            "harness.observed",
            Some(agent),
            json!({"state": "idle", "incarnation_id": "inc-1", "observed_at_ms": 2_000}),
        ),
    ] {
        upgraded
            .append_claim(&ClaimInput {
                subject: agent.into(),
                kind: kind.into(),
                actor: actor.map(str::to_owned),
                fields: serde_json::from_value(fields).unwrap(),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some(format!("after-upgrade:{kind}")),
            })
            .unwrap();
    }
    let joined = Store::open_memory("joined").unwrap();
    receive_and_project(
        &joined,
        "writer",
        &exchange_from(&upgraded, &joined.replication_inventory().unwrap()),
    );

    let run_id = upgraded
        .readers
        .get()
        .query_row("SELECT id FROM mission_runs", [], |row| {
            row.get::<_, String>(0)
        })
        .unwrap();
    let graph = |store: &Store| {
        let run = store.mission_run(&run_id).unwrap().unwrap();
        let steps = run
            .steps
            .iter()
            .map(|step| (step.step.clone(), step.status.clone(), step.attempt))
            .collect::<Vec<_>>();
        let status = store
            .replication_status(true, Some(TEST_FLEET), &[])
            .unwrap();
        (
            run.status,
            run.phase,
            steps,
            status.authority_digest,
            status.graph_digest,
        )
    };
    let (status, phase, steps, authority, digest) = graph(&upgraded);
    assert_eq!((status.as_str(), phase.as_str()), ("running", "normal"));
    assert_eq!(
        steps,
        [
            ("announce", "pending", 1),
            ("deploy-check", "ready", 2),
            ("prepare", "completed", 1),
            ("report", "pending", 1),
            ("smoke-check", "pending", 1),
        ]
        .map(|(step, status, attempt)| (
            step.to_owned(),
            status.to_owned(),
            attempt
        ))
    );
    assert_eq!(graph(&joined), (status, phase, steps, authority, digest));
}

/// A replay over existing rows used to re-apply a reopened run's old terminal state, so the
/// node that drives the run treated it as over. Before its first canonical replay, that node
/// writes the claims that keep the run over, rather than start its work again. A replica
/// leaves the run to the node that drives it.
#[test]
fn a_run_an_old_replay_ended_stays_over_with_claims_that_say_so() {
    let source = Store::open_memory("source").unwrap();
    let replica = Store::open_memory("replica").unwrap();
    let failed = failed_takeover_run(&source, &["deploy-check"]);
    let revised = publish_takeover(
        &source,
        &TAKEOVER_SOURCE.replace(
            r#"step "smoke-check" { depends-on"#,
            r#"step "smoke-check" { goal "Probe the new health endpoint."; depends-on"#,
        ),
        "takeover-revision",
    );
    source
        .adopt_mission_revision(
            &failed.id,
            &revised,
            "person/requester",
            "the smoke check probes the new endpoint",
            "revise-failed-takeover",
        )
        .unwrap();
    receive_and_project(
        &replica,
        "source",
        &exchange_from(&source, &replica.replication_inventory().unwrap()),
    );
    // The replay before this change, over the rows each node had projected.
    let replay_over_existing_rows = |store: &Store| {
        let mut connection = store.connection.lock().unwrap();
        let transaction = connection.transaction().unwrap();
        rebuild_operations_tx(&transaction).unwrap();
        project_replicated_base_claims(&transaction).unwrap();
        project_replicated_mission_runs(&transaction).unwrap();
        rebuild_planning_tx(&transaction).unwrap();
        transaction.commit().unwrap();
    };
    let run_state = |store: &Store| {
        let run = store.mission_run(&failed.id).unwrap().unwrap();
        (run.status, run.phase)
    };
    for store in [&source, &replica] {
        replay_over_existing_rows(store);
        assert_eq!(run_state(store), ("failed".into(), "terminal".into()));
    }

    assert_eq!(
        replica.settle_runs_for_canonical_replay().unwrap(),
        Vec::<String>::new()
    );
    assert_eq!(run_state(&replica), ("running".into(), "normal".into()));
    assert_eq!(
        source.settle_runs_for_canonical_replay().unwrap(),
        std::slice::from_ref(&failed.id)
    );
    assert_eq!(run_state(&source), ("failed".into(), "terminal".into()));
    assert!(
        source
            .settle_runs_for_canonical_replay()
            .unwrap()
            .is_empty()
    );

    receive_and_project(
        &replica,
        "source",
        &exchange_from(&source, &replica.replication_inventory().unwrap()),
    );
    let late = Store::open_memory("late").unwrap();
    receive_and_project(
        &late,
        "source",
        &exchange_from(&source, &late.replication_inventory().unwrap()),
    );
    let graph = |store: &Store| {
        let run = store.mission_run(&failed.id).unwrap().unwrap();
        let steps = run
            .steps
            .iter()
            .map(|step| (step.step.clone(), step.status.clone(), step.attempt))
            .collect::<Vec<_>>();
        let digest = store
            .replication_status(true, Some(TEST_FLEET), &[])
            .unwrap()
            .graph_digest;
        (run.status, run.phase, steps, digest)
    };
    let expected = graph(&source);
    assert!(
        expected
            .2
            .iter()
            .all(|(_, status, _)| matches!(status.as_str(), "completed" | "cancelled")),
        "{expected:?}"
    );
    assert_eq!(graph(&replica), expected, "replica");
    assert_eq!(graph(&late), expected, "late");
}

#[test]
fn a_person_sets_the_outcome_of_a_finished_run_and_every_node_shows_it() {
    let source = Store::open_memory("source").unwrap();
    let failed = failed_takeover_run(&source, &["deploy-check"]);
    let reason = "the deploy shipped after the check host came back";
    let completed = source
        .set_mission_run_outcome(
            &failed.subject,
            "completed",
            "person/operator",
            reason,
            "outcome-completed",
        )
        .unwrap();
    assert_eq!(
        (completed.status.as_str(), completed.phase.as_str()),
        ("completed", "terminal")
    );
    let outcome = completed
        .outcome
        .clone()
        .expect("the run shows its outcome");
    assert_eq!(outcome.status, "completed");
    assert_eq!(outcome.previous_status.as_deref(), Some("failed"));
    assert_eq!(outcome.actor, "person/operator");
    assert_eq!(outcome.reason, reason);
    assert_eq!(
        takeover_step(&completed, "deploy-check").status,
        "failed",
        "the steps stay as they ended"
    );
    let claim = source
        .latest_claim(&failed.subject, Some("mission-run.state"))
        .unwrap()
        .unwrap();
    assert_eq!(claim.actor.as_deref(), Some("person/operator"));
    assert_eq!(outcome.at_unix_ms, claim.accepted_at_unix_ms);
    let states = source.mission_run_states().unwrap();
    assert_eq!(
        states[&failed.subject].since_unix_ms,
        claim.accepted_at_unix_ms
    );
    assert_eq!(states[&failed.subject].outcome.as_ref(), Some(&outcome));
    assert_eq!(
        source
            .set_mission_run_outcome(
                &failed.subject,
                "completed",
                "person/operator",
                reason,
                "outcome-completed",
            )
            .unwrap()
            .outcome,
        Some(outcome.clone()),
        "the same request is answered from its first result"
    );

    let target = Store::open_memory("target").unwrap();
    target
        .import_replication("source", &source.export_replication(0).unwrap())
        .unwrap();
    let replicated = target.mission_run(&failed.id).unwrap().unwrap();
    assert_eq!(
        (replicated.status.as_str(), replicated.phase.as_str()),
        ("completed", "terminal")
    );
    assert_eq!(replicated.outcome, Some(outcome));
    assert_eq!(
        target.mission_run_states().unwrap()[&failed.subject].since_unix_ms,
        claim.accepted_at_unix_ms
    );

    let rolled_back = source
        .set_mission_run_outcome(
            &failed.subject,
            "failed",
            "person/operator",
            "the deploy was rolled back",
            "outcome-failed",
        )
        .unwrap();
    assert_eq!(rolled_back.status, "failed");
    assert_eq!(
        rolled_back
            .outcome
            .and_then(|outcome| outcome.previous_status),
        Some("completed".into())
    );
}

#[test]
fn only_a_finished_root_run_takes_a_different_outcome_with_a_reason() {
    let store = Store::open_memory("node").unwrap();
    publish_takeover(&store, TAKEOVER_SOURCE, "takeover-mission");
    let running = store
        .create_mission_run(&MissionRunRequest {
            mission: "takeover".into(),
            revision: None,
            workspace: "/tmp".into(),
            requester: Some("person/requester".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "running-takeover".into(),
        })
        .unwrap();
    let refusal = |store: &Store, run: &str, status: &str, reason: &str, key: &str| {
        store
            .set_mission_run_outcome(run, status, "person/operator", reason, key)
            .unwrap_err()
            .code
    };
    assert_eq!(
        refusal(&store, &running.subject, "completed", "done", "running"),
        "mission-run-not-finished"
    );
    assert!(
        store
            .latest_claim(&running.subject, Some("mission-run.state"))
            .unwrap()
            .is_none()
    );

    let store = Store::open_memory("node").unwrap();
    let failed = failed_takeover_run(&store, &["deploy-check"]);
    assert_eq!(
        refusal(
            &store,
            &failed.subject,
            "running",
            "reopen",
            "running-state"
        ),
        "invalid-run-outcome"
    );
    assert_eq!(
        refusal(&store, &failed.subject, "completed", "  ", "no-reason"),
        "missing-outcome-reason"
    );
    assert_eq!(
        refusal(&store, &failed.subject, "failed", "still failed", "same"),
        "mission-run-outcome-unchanged"
    );
    assert_eq!(
        refusal(
            &store,
            "mission-run/missing",
            "completed",
            "done",
            "missing"
        ),
        "missing-mission-run"
    );
    let unchanged = store.mission_run(&failed.id).unwrap().unwrap();
    assert_eq!(unchanged.status, "failed");
    assert!(unchanged.outcome.is_none());
}

#[test]
fn a_retired_mission_leaves_the_lists_keeps_its_history_and_returns_when_published() {
    let store = Store::open_memory("node").unwrap();
    let ready = publish_takeover(&store, TAKEOVER_SOURCE, "takeover-mission");
    let listed = |store: &Store, history: bool| {
        store
            .mission_collection_ids(history, 0, 50)
            .unwrap()
            .contains(&"mission/takeover".to_owned())
    };
    assert!(listed(&store, false));

    let retired = store
        .retire_mission("mission/takeover", "person/operator", "retire-takeover")
        .unwrap();
    assert_eq!(retired.state, MissionState::Retired);
    assert_ne!(retired.revision, ready.revision);
    assert!(!listed(&store, false), "a retired mission leaves the list");
    assert!(listed(&store, true), "its history still lists it");
    assert_eq!(
        store
            .mission_spec("takeover", Some(&ready.revision))
            .unwrap()
            .unwrap()
            .state,
        MissionState::Ready,
        "the ready revision stays in the history"
    );
    let publication = store
        .latest_claim("mission/takeover", Some("mission.published"))
        .unwrap()
        .unwrap();
    assert_eq!(publication.actor.as_deref(), Some("person/operator"));
    assert_eq!(
        store
            .create_mission_run(&MissionRunRequest {
                mission: "takeover".into(),
                revision: None,
                workspace: "/tmp".into(),
                requester: Some("person/requester".into()),
                mode: Some("run".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "retired-run".into(),
            })
            .unwrap_err()
            .code,
        "mission-retired",
        "a retired mission does not start"
    );
    assert_eq!(
        store
            .retire_mission("mission/takeover", "person/operator", "retire-takeover")
            .unwrap(),
        retired,
        "the same request is answered from its first result"
    );
    assert_eq!(
        store
            .retire_mission("takeover", "person/operator", "retire-again")
            .unwrap_err()
            .code,
        "mission-already-retired"
    );

    let target = Store::open_memory("target").unwrap();
    target
        .import_replication("node", &store.export_replication(0).unwrap())
        .unwrap();
    assert_eq!(
        target
            .mission_spec("takeover", None)
            .unwrap()
            .unwrap()
            .state,
        MissionState::Retired
    );
    assert!(!listed(&target, false));

    publish_takeover(&store, TAKEOVER_SOURCE, "takeover-mission-again");
    assert_eq!(
        store
            .mission_spec("takeover", None)
            .unwrap()
            .unwrap()
            .revision,
        ready.revision
    );
    assert!(listed(&store, false), "publishing it again brings it back");
}

#[test]
fn a_mission_with_an_active_run_is_not_retired() {
    let store = Store::open_memory("node").unwrap();
    publish_takeover(&store, TAKEOVER_SOURCE, "takeover-mission");
    let run = store
        .create_mission_run(&MissionRunRequest {
            mission: "takeover".into(),
            revision: None,
            workspace: "/tmp".into(),
            requester: Some("person/requester".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "active-takeover".into(),
        })
        .unwrap();
    let refused = store
        .retire_mission("takeover", "person/operator", "retire-active")
        .unwrap_err();
    assert_eq!(refused.code, "mission-has-active-run");
    assert!(refused.message.contains(&run.subject));
    assert_eq!(
        store.mission_spec("takeover", None).unwrap().unwrap().state,
        MissionState::Ready
    );
}

#[test]
fn step_views_carry_the_current_attempts_progress_and_completion_summaries() {
    let store = Store::open_memory("node").unwrap();
    let source = r#"version 2

agent "worker" { workspace "/tmp"; command "true" }

mission "summaries" state="ready" {
  goal "Show what the worker reported."
  step "work" {
    assigned-to "agent/worker"
    retry { attempts 2 }
    gate "the result is ready" { exists "resource/result" }
  }
}
"#;
    let intent = crate::graph::parse_test_intent(source, "node").unwrap();
    let planned = store
        .mission(
            &intent,
            IntentInput {
                kdl: source.into(),
                source_name: None,
            },
        )
        .unwrap();
    store
        .apply(&intent, &planned.subject_tokens, "summaries-mission")
        .unwrap();
    let run = store
        .create_mission_run(&MissionRunRequest {
            mission: "summaries".into(),
            revision: None,
            workspace: "/tmp".into(),
            requester: Some("person/test".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "summaries-run".into(),
        })
        .unwrap();
    let subject = &run.steps[0].subject;
    let request = |key: &str, summary: Option<&str>| WorkRequest {
        wait_ms: None,
        actor: Some("agent/node.worker".into()),
        incarnation: Some("current".into()),
        summary: summary.map(str::to_owned),
        reason: None,
        evidence: Vec::new(),
        idempotency_key: key.into(),
    };

    store.set_step_state(subject, "ready", None).unwrap();
    let claimed = store
        .work_action(subject, "claim", &request("claim-1", Some("Starting")))
        .unwrap();
    assert_eq!(claimed.progress_summary, None, "a claim is not progress");
    let fresh = serde_json::to_value(&claimed).unwrap();
    for field in [
        "progress_summary",
        "progress_at_unix_ms",
        "completion_summary",
    ] {
        assert!(
            fresh.get(field).is_none(),
            "{field} is absent until reported"
        );
    }
    for (key, summary) in [
        ("progress-1", Some("Reading the renderer")),
        ("progress-2", Some("  Tests pass\n")),
        ("progress-3", None),
    ] {
        store
            .work_action(subject, "progress", &request(key, summary))
            .unwrap();
    }
    let working = store.step_run(subject).unwrap().unwrap();
    assert_eq!(working.progress_summary.as_deref(), Some("Tests pass"));
    assert!(working.progress_at_unix_ms.is_some());
    assert_eq!(working.completion_summary, None);

    store
        .work_action(
            subject,
            "complete",
            &request("complete-1", Some("Opened the pull request")),
        )
        .unwrap();
    let shown = store.mission_run(&run.subject).unwrap().unwrap();
    let submitted = &shown.steps[0];
    assert_eq!(submitted.status, "verifying");
    assert_eq!(submitted.progress_summary.as_deref(), Some("Tests pass"));
    assert_eq!(
        submitted.completion_summary.as_deref(),
        Some("Opened the pull request")
    );

    store
        .set_step_state(subject, "failed", Some("the gate failed"))
        .unwrap();
    store.retry_step(subject, "retry attempt 2", 0).unwrap();
    let retried = store.step_run(subject).unwrap().unwrap();
    assert_eq!(retried.attempt, 2);
    assert_eq!(
        retried.progress_summary, None,
        "attempt 1 progress is stale"
    );
    assert_eq!(retried.completion_summary, None);
}

#[test]
fn a_constraint_revision_preserves_unrelated_work_and_resets_dependents() {
    let store = Store::open_memory("node").unwrap();
    let publish = |source: &str, key: &str| {
        let intent = crate::graph::parse_test_intent(source, "node").unwrap();
        let planned = store
            .mission(
                &intent,
                IntentInput {
                    kdl: source.into(),
                    source_name: None,
                },
            )
            .unwrap();
        store.apply(&intent, &planned.subject_tokens, key).unwrap();
        intent.missions["revision"].clone()
    };
    let first = publish(
        r#"
version 2

  mission "revision" state="ready" {
    goal "Complete mission revision."
    agent "worker" { workspace "."; command "true" }
    step "owned" { assigned-to "agent/worker"; goal "First goal." }
    step "unrelated" { }
    step "join" { depends-on { step "owned" completed; step "unrelated" completed } }
  }

"#,
        "revision-one",
    );
    let run = store
        .create_mission_run(&MissionRunRequest {
            mission: first.id,
            revision: None,
            workspace: "/tmp".into(),
            requester: Some("person/test".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "revision-run".into(),
        })
        .unwrap();
    let worker = format!("agent/{}/worker", run.id);
    for step in &run.steps {
        store
            .set_step_state(&step.subject, "completed", None)
            .unwrap();
    }
    let second = publish(
        r#"
version 2

  mission "revision" state="ready" {
    goal "Complete mission revision."
    agent "worker" { workspace "."; command "true" }
    step "owned" {
      assigned-to "agent/worker"
      goal "First goal."
      constraint "Do not push."
    }
    step "unrelated" { }
    step "join" { depends-on { step "owned" completed; step "unrelated" completed } }
  }

"#,
        "revision-two",
    );
    let revised = store
        .adopt_mission_revision(
            &run.id,
            &second,
            &worker,
            "the work needs a new constraint",
            "adopt-revision-two",
        )
        .unwrap();
    let states = revised
        .steps
        .iter()
        .map(|step| (step.step.as_str(), step.status.as_str()))
        .collect::<BTreeMap<_, _>>();
    assert_eq!(states["owned"], "pending");
    assert_eq!(states["join"], "pending");
    assert_eq!(states["unrelated"], "completed");
    assert_eq!(
        revised
            .steps
            .iter()
            .find(|step| step.step == "owned")
            .unwrap()
            .constraints,
        ["Do not push."]
    );
    assert_eq!(revised.root_revision, run.root_revision);
    assert_eq!(revised.revision, second.revision);
}

#[test]
fn a_mission_constraint_revision_preserves_completed_and_submitted_work() {
    let store = Store::open_memory("node").unwrap();
    let publish = |constraint: &str, key: &str| {
        let source = format!(
            r#"
version 2

  mission "revision-delivery" state="ready" {{
    goal "Keep delivered work across mission guidance edits."
    agent "worker" {{ workspace "."; command "true" }}
    constraint {constraint:?}
    step "completed" {{ assigned-to "agent/${{ST_MISSION_RUN}}/worker"; goal "Finish once." }}
    step "submitted" {{ assigned-to "agent/${{ST_MISSION_RUN}}/worker"; goal "Verify once." }}
  }}

"#
        );
        let intent = crate::graph::parse_test_intent(&source, "node").unwrap();
        let planned = store
            .mission(
                &intent,
                IntentInput {
                    kdl: source,
                    source_name: None,
                },
            )
            .unwrap();
        store.apply(&intent, &planned.subject_tokens, key).unwrap();
        intent.missions["revision-delivery"].clone()
    };
    let first = publish("Use the first mission constraint.", "delivery-one");
    let run = store
        .create_mission_run(&MissionRunRequest {
            mission: first.id,
            revision: None,
            workspace: "/tmp".into(),
            requester: Some("person/test".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "delivery-run".into(),
        })
        .unwrap();
    let worker = format!("agent/{}/worker", run.id);
    let completed = run
        .steps
        .iter()
        .find(|step| step.step == "completed")
        .unwrap()
        .subject
        .clone();
    store.set_step_state(&completed, "completed", None).unwrap();
    let submitted = run
        .steps
        .iter()
        .find(|step| step.step == "submitted")
        .unwrap()
        .subject
        .clone();
    store.set_step_state(&submitted, "ready", None).unwrap();
    let request = |key: &str| WorkRequest {
        wait_ms: None,
        actor: Some(worker.clone()),
        incarnation: Some("worker-one".into()),
        summary: Some("the result is ready for verification".into()),
        reason: None,
        evidence: Vec::new(),
        idempotency_key: key.into(),
    };
    store
        .work_action(&submitted, "claim", &request("delivery-claim"))
        .unwrap();
    store
        .work_action(&submitted, "complete", &request("delivery-submit"))
        .unwrap();

    let second = publish(
        "Use the revised mission constraint without redoing delivered work.",
        "delivery-two",
    );
    let revised = store
        .adopt_mission_revision(
            &run.id,
            &second,
            "person/test",
            "clarify mission-wide guidance",
            "delivery-cutover",
        )
        .unwrap();

    let completed = revised
        .steps
        .iter()
        .find(|step| step.step == "completed")
        .unwrap();
    assert_eq!(completed.status, "completed");
    let submitted = revised
        .steps
        .iter()
        .find(|step| step.step == "submitted")
        .unwrap();
    assert_eq!(submitted.status, "verifying");
    assert!(submitted.worker_reported);
    assert!(submitted.claimant.is_none());
    assert!(submitted.claim_incarnation.is_none());
}

#[test]
fn a_revision_keeps_the_old_generation_and_fences_its_work() {
    let store = Store::open_memory("node").unwrap();
    let publish = |source: &str, key: &str| {
        let intent = crate::graph::parse_test_intent(source, "node").unwrap();
        let planned = store
            .mission(
                &intent,
                IntentInput {
                    kdl: source.into(),
                    source_name: None,
                },
            )
            .unwrap();
        store.apply(&intent, &planned.subject_tokens, key).unwrap();
        intent.missions["generation"].clone()
    };
    let first = publish(
        r#"
version 2

  mission "generation" state="ready" {
    goal "Test immutable generations."
    agent "worker" { workspace "."; command "true" }
    step "active" { assigned-to "agent/${ST_MISSION_RUN}/worker"; goal "Keep this definition." }
    step "stable" { goal "Carry this result." }
    step "changed" { goal "Use the first definition." }
    step "dependent" {
      depends-on { step "changed" completed }
      goal "Depend on the changed step."
    }
  }

"#,
        "generation-one",
    );
    let run = store
        .create_mission_run(&MissionRunRequest {
            mission: first.id,
            revision: None,
            workspace: "/tmp".into(),
            requester: Some("person/test".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "generation-run".into(),
        })
        .unwrap();
    let worker = format!("agent/{}/worker", run.id);
    let active = run
        .steps
        .iter()
        .find(|step| step.step == "active")
        .unwrap()
        .subject
        .clone();
    store.set_step_state(&active, "ready", None).unwrap();
    let request = |key: &str| WorkRequest {
        wait_ms: None,
        actor: Some(worker.clone()),
        incarnation: Some("worker-one".into()),
        summary: None,
        reason: None,
        evidence: Vec::new(),
        idempotency_key: key.into(),
    };
    store
        .work_action(&active, "claim", &request("generation-claim"))
        .unwrap();
    store
        .work_action(&active, "progress", &request("generation-progress"))
        .unwrap();
    for path in ["stable", "changed", "dependent"] {
        let subject = &run
            .steps
            .iter()
            .find(|step| step.step == path)
            .unwrap()
            .subject;
        store.set_step_state(subject, "completed", None).unwrap();
    }
    let second = publish(
        r#"
version 2

  mission "generation" state="ready" {
    goal "Test immutable generations."
    agent "worker" { workspace "."; command "true" }
    step "active" { assigned-to "agent/${ST_MISSION_RUN}/worker"; goal "Keep this definition." }
    step "stable" { goal "Carry this result." }
    step "changed" { goal "Use the second definition." }
    step "dependent" {
      depends-on { step "changed" completed }
      goal "Depend on the changed step."
    }
  }

"#,
        "generation-two",
    );
    let revised = store
        .adopt_mission_revision(
            &run.id,
            &second,
            &worker,
            "the work now needs the second definition",
            "generation-cutover",
        )
        .unwrap();

    assert_ne!(revised.generation, run.generation);
    assert_eq!(revised.initial_revision, run.revision);
    assert_eq!(
        store
            .mission_specs_for_runs(&[revised.subject.clone()])
            .unwrap()[&revised.subject]
            .revision,
        revised.revision
    );
    let generations = store.run_generations(&run.id).unwrap();
    assert_eq!(generations.len(), 2);
    assert_eq!(generations[0].subject, run.generation);
    assert_eq!(generations[0].status, "superseded");
    assert_eq!(
        generations[0]
            .steps
            .iter()
            .find(|step| step.step == "active")
            .unwrap()
            .status,
        "cancelled"
    );
    let retired = generations[0]
        .steps
        .iter()
        .find(|step| step.step == "active")
        .unwrap();
    assert!(retired.claimant.is_none());
    assert!(retired.claim_incarnation.is_none());
    assert!(retired.claim_expires_at_unix_ms.is_none());
    assert_eq!(
        generations[1].predecessor.as_deref(),
        Some(run.generation.as_str())
    );
    let states = revised
        .steps
        .iter()
        .map(|step| (step.step.as_str(), step.status.as_str()))
        .collect::<BTreeMap<_, _>>();
    assert_eq!(states["active"], "working");
    assert_eq!(states["stable"], "completed");
    assert_eq!(states["changed"], "pending");
    assert_eq!(states["dependent"], "pending");
    assert!(
        revised
            .steps
            .iter()
            .all(|step| step.generation == revised.generation)
    );
    let carried = revised
        .steps
        .iter()
        .find(|step| step.step == "active")
        .unwrap();
    assert_eq!(carried.claimant.as_deref(), Some(worker.as_str()));
    assert_eq!(carried.claim_incarnation.as_deref(), Some("worker-one"));
    let stranger = WorkRequest {
        wait_ms: None,
        actor: Some(format!("agent/{}/stranger", run.id)),
        ..request("stale-generation-complete")
    };
    let error = store
        .work_action(&active, "complete", &stranger)
        .unwrap_err();
    assert_eq!(error.code, "stale-run-generation");
    assert!(!store.set_step_state(&active, "completed", None).unwrap());
    assert_eq!(
        store.step_run(&active).unwrap().unwrap().status,
        "cancelled"
    );
    let submitted = store
        .work_action(&active, "complete", &request("carried-complete"))
        .unwrap();
    assert_eq!(submitted.subject, carried.subject);
    assert_eq!(submitted.status, "verifying");
    let retried = store
        .adopt_mission_revision(
            &run.id,
            &second,
            &worker,
            "the work now needs the second definition",
            "generation-cutover",
        )
        .unwrap();
    assert_eq!(retried.generation, revised.generation);
}

#[test]
fn an_agent_revises_outside_its_graph_location_as_itself() {
    let store = Store::open_memory("node").unwrap();
    let publish = |source: &str, key: &str| {
        let intent = crate::graph::parse_test_intent(source, "node").unwrap();
        let planned = store
            .mission(
                &intent,
                IntentInput {
                    kdl: source.into(),
                    source_name: None,
                },
            )
            .unwrap();
        store.apply(&intent, &planned.subject_tokens, key).unwrap();
        intent.missions["authority"].clone()
    };
    let first = publish(
        r#"
version 2

  mission "authority" state="ready" {
    goal "Keep revision authority structural."
    step "work" { assigned-to "agent/node.worker"; goal "Use the first goal." }
  }

"#,
        "authority-one",
    );
    let run = store
        .create_mission_run(&MissionRunRequest {
            mission: first.id,
            revision: None,
            workspace: "/tmp".into(),
            requester: Some("person/test".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "authority-run".into(),
        })
        .unwrap();
    let worker = format!("agent/{}/worker", run.id);
    let escalated = publish(
        r#"
version 2

  mission "authority" state="ready" {
    goal "Keep revision authority structural."
    step "work" {
      assigned-to "agent/node.worker"
      goal "Use the second goal."
       agent "worker" { workspace "."; command "true" }
    }
  }

"#,
        "authority-two",
    );
    // Free mode: an agent revises any part of a run, and the revision records it as actor.
    let revised = store
        .adopt_mission_revision(
            &run.id,
            &escalated,
            &worker,
            "declare a seat in the candidate graph",
            "authority-cutover",
        )
        .unwrap();
    assert_ne!(revised.generation, run.generation);
    let generation = store.run_generation(&revised.generation).unwrap().unwrap();
    assert_eq!(generation.actor, worker);
}

const CARRY_SOURCE: &str = r#"
version 2

  mission "carry" state="ready" {
    goal "Keep claimed work across a revision."
    step "work" { assigned-to "agent/worker"; goal "Do the claimed work." }
    step "other" { assigned-to "agent/worker"; goal "Use the first goal." }
  }

"#;

fn publish_carry(store: &Store, source: &str, key: &str) -> MissionSpec {
    let intent = crate::graph::parse_test_intent(source, "node").unwrap();
    let planned = store
        .mission(
            &intent,
            IntentInput {
                kdl: source.into(),
                source_name: None,
            },
        )
        .unwrap();
    store.apply(&intent, &planned.subject_tokens, key).unwrap();
    intent.missions["carry"].clone()
}

fn carry_request(incarnation: &str, key: &str) -> WorkRequest {
    WorkRequest {
        wait_ms: None,
        actor: Some("agent/node.worker".into()),
        incarnation: Some(incarnation.into()),
        summary: None,
        reason: None,
        evidence: Vec::new(),
        idempotency_key: key.into(),
    }
}

#[test]
fn revise_notes_carry_progress_for_active_and_completed_steps() {
    let store = Store::open_memory("node").unwrap();
    let source = CARRY_SOURCE.replace(
        "step \"other\"",
        "step \"done\" { assigned-to \"agent/worker\"; goal \"Finish the work.\" }\n    step \"other\"",
    );
    publish_carry(&store, &source, "notes-one");
    let run = store
        .create_mission_run(&MissionRunRequest {
            mission: "carry".into(),
            revision: None,
            workspace: "/tmp".into(),
            requester: Some("person/requester".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "notes-run".into(),
        })
        .unwrap();
    let step = |run: &MissionRunView, path: &str| {
        run.steps
            .iter()
            .find(|step| step.step == path)
            .unwrap()
            .clone()
    };
    let report = |subject: &str, action: &str, key: &str, summary: &str| {
        let mut request = carry_request("one", key);
        request.summary = Some(summary.into());
        store.work_action(subject, action, &request).unwrap()
    };
    let done = step(&run, "done").subject;
    store.set_step_state(&done, "ready", None).unwrap();
    report(&done, "claim", "notes-claim-done", "Starting");
    report(&done, "progress", "notes-done", "Decision: keep the result");
    report(&done, "complete", "notes-complete", "The result is ready");
    store.set_step_state(&done, "completed", None).unwrap();
    let work = step(&run, "work").subject;
    store.set_step_state(&work, "ready", None).unwrap();
    report(&work, "claim", "notes-claim-work", "Starting");
    report(&work, "progress", "notes-work", "Checking the result");
    let original = store.mission_run(&run.subject).unwrap().unwrap();
    let mut current = original.clone();
    for revision in 2..=3 {
        let next = publish_carry(
            &store,
            &source.replace("Use the first goal.", &format!("Use goal {revision}.")),
            &format!("notes-publish-{revision}"),
        );
        current = store
            .adopt_mission_revision(
                &run.id,
                &next,
                "person/requester",
                "adjust the other step",
                &format!("notes-revise-{revision}"),
            )
            .unwrap();
        for path in ["work", "done"] {
            let before = step(&original, path);
            let after = step(&current, path);
            assert_ne!(before.subject, after.subject);
            assert_eq!(after.status, before.status);
            assert_eq!(after.progress_summary, before.progress_summary, "{path}");
            assert_eq!(
                after.progress_at_unix_ms, before.progress_at_unix_ms,
                "{path}"
            );
            assert_eq!(
                after.completion_summary, before.completion_summary,
                "{path}"
            );
            assert_eq!(
                after.submitted_incarnation, before.submitted_incarnation,
                "{path}"
            );
        }
    }
    let carried = step(&current, "work");
    let updated = report(
        &carried.subject,
        "progress",
        "notes-new-progress",
        "The check passed",
    );
    assert_eq!(
        updated.progress_summary.as_deref(),
        Some("The check passed")
    );
    let target = Store::open_memory("target").unwrap();
    target
        .import_replication("node", &store.export_replication(0).unwrap())
        .unwrap();
    for path in ["work", "done"] {
        let expected = store
            .step_run(&step(&current, path).subject)
            .unwrap()
            .unwrap();
        let replicated = target.step_run(&expected.subject).unwrap().unwrap();
        assert_eq!(replicated.progress_summary, expected.progress_summary);
        assert_eq!(replicated.progress_at_unix_ms, expected.progress_at_unix_ms);
        assert_eq!(replicated.completion_summary, expected.completion_summary);
    }
    store
        .set_step_state(&carried.subject, "failed", Some("try again"))
        .unwrap();
    store
        .retry_step(&carried.subject, "a new attempt", 0)
        .unwrap();
    let retried = store.step_run(&carried.subject).unwrap().unwrap();
    assert_eq!(
        retried.progress_summary, None,
        "carried notes belong to attempt 1"
    );
    assert_eq!(retried.completion_summary, None);
}

#[test]
fn revise_notes_inheritance_uses_the_cutover_snapshot() {
    let store = Store::open_memory("node").unwrap();
    publish_carry(&store, CARRY_SOURCE, "snapshot-notes-mission");
    let run = store
        .create_mission_run(&MissionRunRequest {
            mission: "carry".into(),
            revision: None,
            workspace: "/tmp".into(),
            requester: Some("person/requester".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "snapshot-notes-run".into(),
        })
        .unwrap();
    let source = &run.steps[0].subject;
    let mut successor = run.steps[1].clone();
    let append = |subject: &str, kind: &str, at: &str, fields: Value| {
        let claim = store
            .append_claim(&ClaimInput {
                subject: subject.into(),
                kind: kind.into(),
                actor: Some("agent/node.worker".into()),
                fields: serde_json::from_value(fields).unwrap(),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        // Deterministic fixture times avoid depending on wall-clock tick boundaries.
        store
            .connection
            .write()
            .execute(
                "UPDATE claims SET accepted_at_unix_ms=?2 WHERE id=?1",
                params![claim.id, at],
            )
            .unwrap();
    };
    append(
        source,
        "work.progress",
        "100",
        json!({"attempt": 1, "summary": "Before cutover"}),
    );
    append(
        source,
        "work.submitted",
        "110",
        json!({"attempt": 1, "summary": "Finished", "submitted_incarnation": "one"}),
    );
    append(
        &successor.subject,
        "step-run.carried",
        "200",
        json!({"attempt": 1, "source_step_run": source}),
    );
    append(
        source,
        "work.progress",
        "300",
        json!({"attempt": 1, "summary": "After cutover"}),
    );
    append(
        &successor.subject,
        "work.progress",
        "400",
        json!({"attempt": 1, "summary": "Successor note"}),
    );
    let connection = store.readers.get();
    enrich_step_summaries_at(&connection, &mut successor, 250).unwrap();
    assert_eq!(
        successor.progress_summary.as_deref(),
        Some("Before cutover")
    );
    assert_eq!(successor.progress_at_unix_ms, Some(100));
    assert_eq!(successor.completion_summary.as_deref(), Some("Finished"));
    assert_eq!(successor.submitted_incarnation.as_deref(), Some("one"));
    enrich_step_summaries_at(&connection, &mut successor, 500).unwrap();
    assert_eq!(
        successor.progress_summary.as_deref(),
        Some("Successor note")
    );
    assert_eq!(successor.progress_at_unix_ms, Some(400));
    successor.attempt = 2;
    enrich_step_summaries_at(&connection, &mut successor, 500).unwrap();
    assert_eq!(successor.progress_summary, None);
    assert_eq!(successor.completion_summary, None);
    assert_eq!(successor.submitted_incarnation, None);
}

#[test]
fn revise_notes_replicated_carries_choose_the_newest_link_and_stop_at_cycles() {
    let store = Store::open_memory("node").unwrap();
    publish_carry(&store, CARRY_SOURCE, "replicated-notes-mission");
    let run = store
        .create_mission_run(&MissionRunRequest {
            mission: "carry".into(),
            revision: None,
            workspace: "/tmp".into(),
            requester: Some("person/requester".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "replicated-notes-run".into(),
        })
        .unwrap();
    let peer = Store::open_memory("peer").unwrap();
    peer.import_replication("node", &store.export_replication(0).unwrap())
        .unwrap();
    let older = &run.steps[0].subject;
    let newer = format!("{older}-newer");
    let mut successor = run.steps[1].clone();
    let append = |store: &Store, subject: &str, kind: &str, fields: Value| {
        store
            .append_claim(&ClaimInput {
                subject: subject.into(),
                kind: kind.into(),
                actor: Some("agent/node.worker".into()),
                fields: serde_json::from_value(fields).unwrap(),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap()
    };
    let older_note = append(
        &store,
        older,
        "work.progress",
        json!({"attempt": 1, "summary": "Older lineage"}),
    );
    let older_link = append(
        &store,
        &successor.subject,
        "step-run.carried",
        json!({"attempt": 1, "source_step_run": older}),
    );
    let newer_note = append(
        &peer,
        &newer,
        "work.progress",
        json!({"attempt": 1, "summary": "Newer lineage"}),
    );
    let newer_link = append(
        &peer,
        &successor.subject,
        "step-run.carried",
        json!({"attempt": 1, "source_step_run": newer}),
    );
    let cycle = append(
        &peer,
        &newer,
        "step-run.carried",
        json!({"attempt": 1, "source_step_run": successor.subject}),
    );
    store
        .import_replication("peer", &peer.export_replication(0).unwrap())
        .unwrap();
    assert_eq!(
        store
            .claims_for(&successor.subject, Some("step-run.carried"))
            .unwrap()
            .len(),
        2
    );
    // Set deterministic snapshot times after replication has preserved both
    // concurrent links. Equal cutover times make the cycle reachable.
    for (claim, at) in [
        (older_note, "100"),
        (older_link, "200"),
        (newer_note, "150"),
        (newer_link, "250"),
        (cycle, "250"),
    ] {
        store
            .connection
            .write()
            .execute(
                "UPDATE claims SET accepted_at_unix_ms=?2 WHERE id=?1",
                params![claim.id, at],
            )
            .unwrap();
    }
    let connection = store.readers.get();
    enrich_step_summaries_at(&connection, &mut successor, 225).unwrap();
    assert_eq!(successor.progress_summary.as_deref(), Some("Older lineage"));
    enrich_step_summaries_at(&connection, &mut successor, 300).unwrap();
    assert_eq!(successor.progress_summary.as_deref(), Some("Newer lineage"));
    assert_eq!(successor.progress_at_unix_ms, Some(150));
    assert_eq!(successor.completion_summary, None);
    assert_eq!(successor.submitted_incarnation, None);
}

#[test]
fn a_gate_only_revision_keeps_submitted_work_and_any_other_change_starts_it_again() {
    let store = Store::open_memory("node").unwrap();
    let source = |gate: &str, goal: &str| {
        format!(
            r#"
version 2

  mission "carry" state="ready" {{
    goal "Keep submitted work across a gate revision."
    step "work" {{
      assigned-to "agent/worker"
      goal {goal:?}
      gate "checked" {{ exec {gate:?}; host "node"; workspace "/tmp" }}
    }}
  }}
"#
        )
    };
    publish_carry(&store, &source("exit 2", "Do the work."), "regate-one");
    let run = store
        .create_mission_run(&MissionRunRequest {
            mission: "carry".into(),
            revision: None,
            workspace: "/tmp".into(),
            requester: Some("person/requester".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "regate-run".into(),
        })
        .unwrap();
    let work = |run: &MissionRunView| {
        run.steps
            .iter()
            .find(|step| step.step == "work")
            .unwrap()
            .clone()
    };
    let first = work(&run).subject;
    store.set_step_state(&first, "ready", None).unwrap();
    store
        .work_action(&first, "claim", &carry_request("one", "regate-claim"))
        .unwrap();
    let submitted = store
        .work_action(&first, "complete", &carry_request("one", "regate-complete"))
        .unwrap();
    assert_eq!(submitted.status, "verifying");

    let regated = publish_carry(&store, &source("exit 0", "Do the work."), "regate-two");
    let adopted = store
        .adopt_mission_revision(
            &run.id,
            &regated,
            "person/requester",
            "the gate was broken",
            "regate-revision",
        )
        .unwrap();
    let carried = work(&adopted);
    assert_ne!(carried.subject, first);
    assert_eq!(
        (
            carried.status.as_str(),
            carried.worker_reported,
            carried.attempt
        ),
        ("verifying", true, 1)
    );

    let rewritten = publish_carry(&store, &source("exit 0", "Do other work."), "regate-three");
    let adopted = store
        .adopt_mission_revision(
            &run.id,
            &rewritten,
            "person/requester",
            "the work changed",
            "regate-goal-revision",
        )
        .unwrap();
    let restarted = work(&adopted);
    assert_eq!(
        (restarted.status.as_str(), restarted.worker_reported),
        ("pending", false)
    );
}

#[test]
fn a_person_revises_a_run_from_outside_its_graph_location() {
    let store = Store::open_memory("node").unwrap();
    publish_carry(&store, CARRY_SOURCE, "carry-one");
    let run = store
        .create_mission_run(&MissionRunRequest {
            mission: "carry".into(),
            revision: None,
            workspace: "/tmp".into(),
            requester: Some("person/requester".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "carry-run".into(),
        })
        .unwrap();
    let revised = publish_carry(
        &store,
        &CARRY_SOURCE.replace("Use the first goal.", "Use the second goal."),
        "carry-two",
    );

    let adopted = store
        .adopt_mission_revision(
            &run.id,
            &revised,
            "person/operator",
            "an operator corrects the goal",
            "person-revision",
        )
        .unwrap();
    assert_eq!(adopted.revision, revised.revision);
}

#[test]
fn a_revision_keeps_the_claim_on_a_carried_step() {
    let store = Store::open_memory("node").unwrap();
    publish_carry(&store, CARRY_SOURCE, "carry-one");
    let run = store
        .create_mission_run(&MissionRunRequest {
            mission: "carry".into(),
            revision: None,
            workspace: "/tmp".into(),
            requester: Some("person/requester".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "carry-run".into(),
        })
        .unwrap();
    let step = |run: &MissionRunView, path: &str| {
        run.steps
            .iter()
            .find(|step| step.step == path)
            .unwrap()
            .clone()
    };
    let old = step(&run, "work").subject;
    store.set_step_state(&old, "ready", None).unwrap();
    store
        .work_action(&old, "claim", &carry_request("one", "carry-claim"))
        .unwrap();
    let revised = publish_carry(
        &store,
        &CARRY_SOURCE.replace("Use the first goal.", "Use the second goal."),
        "carry-two",
    );

    let adopted = store
        .adopt_mission_revision(
            &run.id,
            &revised,
            "person/requester",
            "the other step needs a second goal",
            "carry-revision",
        )
        .unwrap();

    let carried = step(&adopted, "work");
    assert_ne!(carried.subject, old);
    assert_eq!(
        (
            carried.status.as_str(),
            carried.claimant.as_deref(),
            carried.claim_incarnation.as_deref()
        ),
        ("claimed", Some("agent/node.worker"), Some("one"))
    );
    assert!(carried.claim_expires_at_unix_ms.is_some());
    assert_eq!(step(&adopted, "other").status, "pending");

    let target = Store::open_memory("target").unwrap();
    target
        .import_replication("node", &store.export_replication(0).unwrap())
        .unwrap();
    let replicated = target.step_run(&carried.subject).unwrap().unwrap();
    assert_eq!(
        (
            replicated.status.as_str(),
            replicated.claimant.as_deref(),
            replicated.claim_incarnation.as_deref(),
            replicated.claim_expires_at_unix_ms
        ),
        (
            "claimed",
            Some("agent/node.worker"),
            Some("one"),
            carried.claim_expires_at_unix_ms
        )
    );

    let other_incarnation = store
        .work_action(&old, "progress", &carry_request("two", "carry-other"))
        .unwrap_err();
    assert_eq!(other_incarnation.code, "wrong-work-incarnation");
    let reclaim = store
        .work_action(&old, "claim", &carry_request("one", "carry-reclaim"))
        .unwrap_err();
    assert_eq!(reclaim.code, "stale-run-generation");
    let submitted = store
        .work_action(&old, "complete", &carry_request("one", "carry-complete"))
        .unwrap();
    assert_eq!(submitted.subject, carried.subject);
    assert_eq!(submitted.status, "verifying");
}

#[test]
fn a_mission_selector_or_completion_change_restarts_carried_work() {
    let parse = |selector: &str, completion: &str| {
        let source = format!(
            r#"
version 2

  mission "contract" state="ready" {{
    goal "Use the current mission contract."
    {selector}
    {completion}
    step "work" {{ }}
  }}

"#
        );
        parse_intent(&source, "node").unwrap().missions["contract"].clone()
    };
    let old = parse(
        "assigned-to \"agent/node.one\"",
        "completion { when \"all-steps-exhausted\" }",
    );
    let selector_changed = parse(
        "assigned-to \"agent/node.two\"",
        "completion { when \"all-steps-exhausted\" }",
    );
    let completion_changed = parse(
        "assigned-to \"agent/node.one\"",
        "completion { depends-on { step \"work\" completed } }",
    );
    for candidate in [&selector_changed, &completion_changed] {
        let (compatible, _) =
            analyze_mission_revision(&old, candidate, "person/requester").unwrap();
        if std::ptr::eq(candidate, &selector_changed) {
            assert!(compatible.is_empty());
        } else {
            assert_eq!(compatible, BTreeSet::from(["work".into()]));
        }
    }
}

#[test]
fn all_affected_human_reviewers_approve_the_exact_revision_preview() {
    let store = Store::open_memory("node").unwrap();
    let publish = |goal: &str, key: &str| {
        let source = format!(
            r#"
version 2

  mission "protected" state="ready" revisions="human-only" revision-reviewer="person/mission-reviewer" {{
    goal "Test protected revision."
     agent "worker" {{ workspace "."; command "true" }}
    step "work" revisions="human-only" revision-reviewer="person/step-reviewer" {{
      assigned-to "agent/worker"
      goal {goal:?}
    }}
  }}

"#
        );
        let intent = crate::graph::parse_test_intent(&source, "node").unwrap();
        let planned = store
            .mission(
                &intent,
                IntentInput {
                    kdl: source,
                    source_name: None,
                },
            )
            .unwrap();
        store.apply(&intent, &planned.subject_tokens, key).unwrap();
        intent.missions["protected"].clone()
    };
    let first = publish("Use the first goal.", "protected-one");
    let run = store
        .create_mission_run(&MissionRunRequest {
            mission: first.id,
            revision: None,
            workspace: "/tmp".into(),
            requester: Some("person/requester".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "protected-run".into(),
        })
        .unwrap();
    let worker = format!("agent/{}/worker", run.id);
    let second = publish("Use the second goal.", "protected-two");
    let proposal = store
        .create_revision_proposal(
            &run.id,
            &second,
            &worker,
            "the first goal is incomplete",
            "protected-proposal",
        )
        .unwrap();
    assert_eq!(proposal.status, "pending-approval");
    assert_eq!(
        proposal.reviewers,
        vec!["person/mission-reviewer", "person/step-reviewer"]
    );
    assert_eq!(store.attention_items(None).unwrap().len(), 2);
    // A mission view reads only the attention that names a run, and finds the same runs.
    assert_eq!(
        store.human_attention_runs().unwrap(),
        store
            .attention_items(None)
            .unwrap()
            .into_iter()
            .filter_map(|item| item.mission_run)
            .collect::<BTreeSet<_>>()
    );
    assert_eq!(
        store.human_attention_runs().unwrap(),
        BTreeSet::from([run.subject.clone()])
    );
    assert_eq!(
        store
            .attention_items(Some("person/mission-reviewer"))
            .unwrap()
            .len(),
        1
    );
    let error = store
        .approve_revision_proposal(
            &proposal.id,
            "person/mission-reviewer",
            "wrong-preview",
            "protected-wrong-preview",
        )
        .unwrap_err();
    assert_eq!(error.code, "stale-revision-preview");
    let first_approval = store
        .approve_revision_proposal(
            &proposal.id,
            "person/mission-reviewer",
            proposal.preview_hash.as_deref().unwrap(),
            "protected-first-approval",
        )
        .unwrap();
    assert_eq!(first_approval.status, "pending-approval");
    assert_eq!(first_approval.mission_run.generation, run.generation);
    assert!(
        store
            .attention_items(Some("person/mission-reviewer"))
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store
            .attention_items(Some("person/step-reviewer"))
            .unwrap()
            .len(),
        1
    );
    let applied = store
        .approve_revision_proposal(
            &proposal.id,
            "person/step-reviewer",
            proposal.preview_hash.as_deref().unwrap(),
            "protected-final-approval",
        )
        .unwrap();
    assert_eq!(applied.status, "applied");
    assert_ne!(applied.mission_run.generation, run.generation);
    assert_eq!(
        applied
            .proposal
            .as_ref()
            .unwrap()
            .successor_generation
            .as_deref(),
        Some(applied.mission_run.generation.as_str())
    );
    let retried = store
        .approve_revision_proposal(
            &proposal.id,
            "person/step-reviewer",
            proposal.preview_hash.as_deref().unwrap(),
            "protected-final-approval-retry",
        )
        .unwrap();
    assert_eq!(
        retried.mission_run.generation,
        applied.mission_run.generation
    );
}

#[test]
fn cancelling_a_revision_proposal_reopens_the_run_for_one_replacement() {
    let store = Store::open_memory("node").unwrap();
    let publish = |goal: &str, key: &str| {
        let source = format!(
            r#"
version 2

  mission "protected" state="ready" revisions="human-only" {{
    goal "Test revision cancellation."
    step "work" {{ goal {goal:?} }}
  }}

"#
        );
        let intent = crate::graph::parse_test_intent(&source, "node").unwrap();
        let planned = store
            .mission(
                &intent,
                IntentInput {
                    kdl: source,
                    source_name: None,
                },
            )
            .unwrap();
        store.apply(&intent, &planned.subject_tokens, key).unwrap();
        intent.missions["protected"].clone()
    };
    let first = publish("Use the first goal.", "cancel-one");
    let run = store
        .create_mission_run(&MissionRunRequest {
            mission: first.id,
            revision: None,
            workspace: "/tmp".into(),
            requester: Some("person/requester".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "cancel-run".into(),
        })
        .unwrap();
    let second = publish("Use the second goal.", "cancel-two");
    let proposal = store
        .create_revision_proposal(
            &run.id,
            &second,
            "person/requester",
            "the first goal is incomplete",
            "cancel-proposal",
        )
        .unwrap();

    let error = store
        .create_revision_proposal(
            &run.id,
            &second,
            "person/requester",
            "replace the pending proposal",
            "cancel-second-pending",
        )
        .unwrap_err();
    assert_eq!(error.code, "revision-proposal-already-pending");
    let error = store
        .cancel_revision_proposal(
            &proposal.id,
            "agent/node.outsider",
            Some("not authorized"),
            "cancel-unauthorized",
        )
        .unwrap_err();
    assert_eq!(error.code, "revision-cancel-not-authorized");

    let cancelled = store
        .cancel_revision_proposal(
            &proposal.id,
            "person/requester",
            Some("the replacement needs another edit"),
            "cancel-authorized",
        )
        .unwrap();
    assert_eq!(cancelled.status, "cancelled");
    assert_eq!(store.mission_run(&run.id).unwrap().unwrap().phase, "normal");
    assert_eq!(store.run_generations(&run.id).unwrap().len(), 1);

    let replacement = store
        .create_revision_proposal(
            &run.id,
            &second,
            "person/requester",
            "the edited replacement is ready",
            "cancel-replacement",
        )
        .unwrap();
    assert_eq!(replacement.status, "pending-approval");
    assert_ne!(replacement.id, proposal.id);
}

#[test]
fn a_when_idle_revision_drains_without_polling_for_new_work() {
    let store = Store::open_memory("node").unwrap();
    let publish = |cutover: Option<&str>, goal: &str, key: &str| {
        let cutover = cutover
            .map(|value| format!(" revision-cutover={value:?}"))
            .unwrap_or_default();
        let source = format!(
            r#"
version 2

  mission "drain" state="ready"{cutover} {{
    goal "Test a drained cutover."
    agent "worker" {{ workspace "."; command "true" }}
    step "active" {{ assigned-to "agent/${{ST_MISSION_RUN}}/worker"; goal "Keep active work." }}
    step "waiting" {{ assigned-to "agent/${{ST_MISSION_RUN}}/worker"; goal {goal:?} }}
  }}

"#
        );
        let intent = crate::graph::parse_test_intent(&source, "node").unwrap();
        let planned = store
            .mission(
                &intent,
                IntentInput {
                    kdl: source,
                    source_name: None,
                },
            )
            .unwrap();
        store.apply(&intent, &planned.subject_tokens, key).unwrap();
        intent.missions["drain"].clone()
    };
    let first = publish(Some("when-idle"), "Use the first goal.", "drain-one");
    let run = store
        .create_mission_run(&MissionRunRequest {
            mission: first.id,
            revision: None,
            workspace: "/tmp".into(),
            requester: Some("person/test".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "drain-run".into(),
        })
        .unwrap();
    let worker = format!("agent/{}/worker", run.id);
    let active = run.steps.iter().find(|step| step.step == "active").unwrap();
    let waiting = run
        .steps
        .iter()
        .find(|step| step.step == "waiting")
        .unwrap();
    store
        .set_step_state(&active.subject, "ready", None)
        .unwrap();
    store
        .set_step_state(&waiting.subject, "ready", None)
        .unwrap();
    let request = |key: &str| WorkRequest {
        wait_ms: None,
        actor: Some(worker.clone()),
        incarnation: Some("worker-one".into()),
        summary: None,
        reason: None,
        evidence: Vec::new(),
        idempotency_key: key.into(),
    };
    let child_request = |key: &str| WorkRequest {
        wait_ms: None,
        actor: Some("agent/node.worker".into()),
        incarnation: Some("worker-one".into()),
        summary: None,
        reason: None,
        evidence: Vec::new(),
        idempotency_key: key.into(),
    };
    let child_source = r#"
version 2

  agent "worker" { workspace "."; command "true" }
  mission "drain-child" state="ready" {
    goal "Keep descendant work in the idle boundary."
    step "active" { assigned-to "agent/worker" }
  }

"#;
    let child_intent = crate::graph::parse_test_intent(child_source, "node").unwrap();
    let child_planned = store
        .mission(
            &child_intent,
            IntentInput {
                kdl: child_source.into(),
                source_name: None,
            },
        )
        .unwrap();
    store
        .apply(&child_intent, &child_planned.subject_tokens, "drain-child")
        .unwrap();
    let child = store
        .create_child_mission_run(
            &MissionRunRequest {
                mission: "drain-child".into(),
                revision: None,
                workspace: "/tmp".into(),
                requester: Some("person/test".into()),
                mode: Some("run".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "drain-child-run".into(),
            },
            &run,
            &waiting.subject,
            None,
        )
        .unwrap();
    let child_active = &child.steps[0];
    store
        .set_step_state(&child_active.subject, "ready", None)
        .unwrap();
    store
        .work_action(
            &child_active.subject,
            "claim",
            &child_request("drain-child-claim"),
        )
        .unwrap();
    store
        .work_action(
            &child_active.subject,
            "progress",
            &child_request("drain-child-progress"),
        )
        .unwrap();
    store
        .work_action(&active.subject, "claim", &request("drain-claim"))
        .unwrap();
    store
        .work_action(&active.subject, "progress", &request("drain-progress"))
        .unwrap();
    let second = publish(Some("restart-active"), "Use the second goal.", "drain-two");
    let error = store
        .adopt_mission_revision(
            &run.id,
            &second,
            &worker,
            "skip the current cutover rule",
            "drain-direct-adopt",
        )
        .unwrap_err();
    assert_eq!(error.code, "revision-needs-drained-cutover");
    let proposal = store
        .create_revision_proposal(
            &run.id,
            &second,
            &worker,
            "wait for active work",
            "drain-proposal",
        )
        .unwrap();
    assert_eq!(proposal.status, "draining");
    assert_eq!(proposal.cutover, RevisionCutover::WhenIdle);
    assert_eq!(
        store.mission_run(&run.id).unwrap().unwrap().phase,
        "revision-draining"
    );
    assert_eq!(store.work(None, false).unwrap().len(), 2);
    let error = store
        .work_action(&waiting.subject, "claim", &request("drain-blocked-claim"))
        .unwrap_err();
    assert_eq!(error.code, "run-generation-draining");
    assert!(store.apply_drained_revision(&run.id).unwrap().is_none());
    store
        .work_action(&active.subject, "complete", &request("drain-complete"))
        .unwrap();
    assert!(store.apply_drained_revision(&run.id).unwrap().is_none());
    store
        .set_step_state(&active.subject, "completed", None)
        .unwrap();
    assert!(store.apply_drained_revision(&run.id).unwrap().is_none());
    store
        .work_action(
            &child_active.subject,
            "complete",
            &child_request("drain-child-complete"),
        )
        .unwrap();
    store
        .set_step_state(&child_active.subject, "completed", None)
        .unwrap();
    let applied = store
        .apply_drained_revision(&run.id)
        .unwrap()
        .expect("the completion event permits cutover");
    assert_eq!(applied.status, "applied");
    assert_eq!(applied.mission_run.phase, "normal");
    assert_ne!(applied.mission_run.generation, run.generation);

    // A late replica can project the draining proposal after the successor
    // generation. It must not put an already cut-over run back into drain.
    let created = store
        .claims_for(&proposal.subject, Some("revision-proposal.created"))
        .unwrap()
        .pop()
        .unwrap();
    {
        let mut connection = store.connection.lock().unwrap();
        let transaction = connection.transaction().unwrap();
        let fields = created.body.get("fields").unwrap_or(&created.body);
        project_revision_proposal(&transaction, &created, fields).unwrap();
        transaction.commit().unwrap();
    }
    assert_eq!(store.mission_run(&run.id).unwrap().unwrap().phase, "normal");
}

#[test]
fn graph_cancellation_enters_final_work_and_is_idempotent() {
    let store = Store::open_memory("node").unwrap();
    let source = r#"
version 2

  agent "worker" { workspace "."; command "true" }
  mission "cancel" state="ready" {
    goal "Cancel this mission through the graph."
    completion { when "all-steps-exhausted" }
    step "work" { assigned-to "agent/node.worker" }
    finally { step "cleanup" { agentless } }
  }

"#;
    let intent = parse_intent(source, "node").unwrap();
    let planned = store
        .mission(
            &intent,
            IntentInput {
                kdl: source.into(),
                source_name: None,
            },
        )
        .unwrap();
    store
        .apply(&intent, &planned.subject_tokens, "publish-cancel-mission")
        .unwrap();
    let run = store
        .create_mission_run(&MissionRunRequest {
            mission: "cancel".into(),
            revision: None,
            workspace: ".".into(),
            requester: Some("person/test".into()),
            mode: None,
            inputs: BTreeMap::new(),
            idempotency_key: "run-cancel-mission".into(),
        })
        .unwrap();
    store
        .append_claim(&ClaimInput {
            subject: "agent/node.worker".into(),
            kind: "harness.observed".into(),
            actor: None,
            fields: BTreeMap::from([
                ("state".into(), Value::String("ready".into())),
                (
                    "incarnation_id".into(),
                    Value::String("worker-incarnation".into()),
                ),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some("cancel-worker-ready".into()),
        })
        .unwrap();
    store
        .set_step_state(
            &run.steps
                .iter()
                .find(|step| step.step == "work")
                .unwrap()
                .subject,
            "ready",
            None,
        )
        .unwrap();
    store
        .work_action(
            &run.steps
                .iter()
                .find(|step| step.step == "work")
                .unwrap()
                .subject,
            "claim",
            &WorkRequest {
                wait_ms: None,
                actor: Some("agent/node.worker".into()),
                incarnation: Some("worker-incarnation".into()),
                summary: None,
                reason: None,
                evidence: Vec::new(),
                idempotency_key: "claim-cancel-work".into(),
            },
        )
        .unwrap();
    let child_source = r#"
version 2

  mission "cancel-child" state="ready" {
    goal "Cancel with the parent run."
    step "child-work" { agentless }
  }

"#;
    let child_intent = parse_intent(child_source, "node").unwrap();
    let child_planned = store
        .mission(
            &child_intent,
            IntentInput {
                kdl: child_source.into(),
                source_name: None,
            },
        )
        .unwrap();
    store
        .apply(
            &child_intent,
            &child_planned.subject_tokens,
            "publish-cancel-child",
        )
        .unwrap();
    let child = store
        .create_child_mission_run(
            &MissionRunRequest {
                mission: "cancel-child".into(),
                revision: None,
                workspace: ".".into(),
                requester: Some("person/test".into()),
                mode: None,
                inputs: BTreeMap::new(),
                idempotency_key: "run-cancel-child".into(),
            },
            &run,
            &run.steps[0].subject,
            None,
        )
        .unwrap();
    let cancellation = format!(
        "version 2\n mission-run {:?} {{ cancellation \"operator\" {{ reason \"the test cancelled the run\" }} }} \n",
        run.subject
    );
    let intent = parse_intent(&cancellation, "node").unwrap();
    let planned = store
        .mission(
            &intent,
            IntentInput {
                kdl: cancellation,
                source_name: None,
            },
        )
        .unwrap();
    let first = store
        .apply(&intent, &planned.subject_tokens, "cancel-mission-run")
        .unwrap();
    let repeated = store
        .apply(&intent, &planned.subject_tokens, "cancel-mission-run")
        .unwrap();
    assert_eq!(first.batch_id, repeated.batch_id);
    let cancelled = store.mission_run(&run.id).unwrap().unwrap();
    assert_eq!(cancelled.status, "running");
    assert_eq!(cancelled.phase, "final-cancelled");
    assert_eq!(
        store.mission_run(&child.id).unwrap().unwrap().phase,
        "cleanup-cancelled"
    );
    assert!(
        !store
            .set_mission_run_state(
                &child.id,
                "standing",
                "normal",
                Some("a stale evaluator tried to reopen the run"),
            )
            .unwrap()
    );
    assert_eq!(
        store.mission_run(&child.id).unwrap().unwrap().phase,
        "cleanup-cancelled"
    );
    assert!(
        !store
            .set_mission_run_state(
                &run.id,
                "standing",
                "normal",
                Some("a stale evaluator tried to reopen the run"),
            )
            .unwrap()
    );
    assert_eq!(
        store.mission_run(&run.id).unwrap().unwrap().phase,
        "final-cancelled"
    );
    assert_eq!(
        cancelled
            .steps
            .iter()
            .find(|step| step.step == "work")
            .unwrap()
            .status,
        "cancelled"
    );
    assert_eq!(
        cancelled
            .steps
            .iter()
            .find(|step| step.step == "cleanup")
            .unwrap()
            .status,
        "pending"
    );
    let cancellation_message = store
        .messages(None, true)
        .unwrap()
        .into_iter()
        .find(|message| message.title.as_deref() == Some("Mission work cancelled"))
        .expect("the claimant receives a cancellation message");
    assert_eq!(cancellation_message.from, "daemon/runtime");
    assert_eq!(cancellation_message.to, "agent/node.worker");
}

#[test]
fn preview_recognizes_a_run_scoped_selector_for_a_declared_agent() {
    let store = Store::open_memory("node").unwrap();
    let source = r#"version 2
mission "work" state="ready" {
  goal "Complete the available work."
  agent "worker" {
    identity "fleet.worker"
    workspace "."
    command "true"
  }
  step "do-work" {
    assigned-to "agent/${ST_MISSION_RUN}/fleet.worker"
  }
}"#;
    let intent = parse_intent(source, "node").unwrap();
    let planned = store
        .mission(
            &intent,
            IntentInput {
                kdl: source.into(),
                source_name: None,
            },
        )
        .unwrap();
    assert!(
        planned
            .warnings
            .iter()
            .all(|warning| !warning.contains("missing eligible agent")),
        "{:?}",
        planned.warnings
    );
}

#[test]
fn preview_warns_when_negative_review_would_cancel_its_only_remediation() {
    let store = Store::open_memory("node").unwrap();
    let source = r#"version 2
mission "review-guardrail" state="ready" {
  goal "Review, remediate, and release."
  step "independent-review" { goal "Fail validation when material findings exist." }
  step "fix-and-real-daemon-proof" {
    goal "Remediate every review finding."
    depends-on { step "independent-review" completed }
  }
}
"#;
    let intent = parse_intent(source, "node").unwrap();
    let planned = store
        .mission(
            &intent,
            IntentInput {
                kdl: source.into(),
                source_name: None,
            },
        )
        .unwrap();
    assert!(planned.blockers.is_empty());
    assert!(
        planned.warnings.iter().any(|warning| {
            warning.contains("failure propagation makes remediation unreachable")
                && warning.contains("independent-review")
        }),
        "{:?}",
        planned.warnings
    );
}

#[test]
fn late_receipt_preserves_observation_and_next_head_push_delivers() {
    let store = Store::open_memory("node").unwrap();
    let spool = tempfile::tempdir().unwrap();
    let repository = "resource/github/acme/demo";
    let subject = format!("{repository}/pull-request/9");
    let observed = json!({
        "number": 9, "state": "open", "title": "Ready", "draft": false,
        "head_sha": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "checks": [{"name": "build", "conclusion": "success"}]
    });
    store.append_claim(&ClaimInput {
        subject: subject.clone(), kind: "resource.observed".into(), actor: None,
        fields: BTreeMap::from([
            ("kind".into(), json!("vcs.pull-request")),
            ("facts".into(), observed.clone()),
        ]), evidence: Vec::new(), expected_subject: None, idempotency_key: None,
    }).unwrap();
    std::fs::write(spool.path().join("1.json"), serde_json::to_vec(&crate::recorder::Receipt {
        schema: "st3.recorder.receipt.v1".into(),
        url: "https://github.com/acme/demo/pull/9".into(),
        actor: "agent/node.builder".into(), mission_run: Some("created".into()),
        exit_code: Some(0), at: "2026-10-03T12:00:00Z".into(),
    }).unwrap()).unwrap();
    assert_eq!(crate::recorder_receipts::ingest_once(&store, spool.path()).unwrap(), 1);
    let recorded = store.latest_actual_value(&subject).unwrap().unwrap()["facts"].clone();
    let previous = json!({"pull_requests": [{
        "number": 9, "state": "open", "draft": false
    }]});
    let current = json!({"pull_requests": [{
        "number": 9, "new": false, "head": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
    }]});
    let items = discovered_collection_items(
        repository, "pull_requests", Some(&previous), &current,
        &mut |_| Some(recorded.clone()),
    );
    assert!(items[0].deliver, "a late receipt must not lose the next head's review");
    for field in ["state", "title", "head_sha", "checks"] {
        assert_eq!(recorded[field], observed[field], "{field}");
    }
    assert_eq!(recorded["opened_by"], "agent/node.builder");
    let projected = store.resource_collection_page(None, None, Some(&subject), None, 10).unwrap();
    assert_eq!(projected[0]["facts"], recorded);
}

#[test]
fn receipt_born_resources_receive_first_observer_issue_and_review_delivery() {
    let store = Store::open_memory("node").unwrap();
    let spool = tempfile::tempdir().unwrap();
    let repository = "resource/github/acme/demo";
    for (field, path, segment) in [
        ("issues", "issues", "issue"),
        ("pull_requests", "pull", "pull-request"),
    ] {
        let receipt = crate::recorder::Receipt {
            schema: "st3.recorder.receipt.v1".into(),
            url: format!("https://github.com/acme/demo/{path}/8"),
            actor: "agent/node.builder".into(),
            mission_run: Some("created".into()),
            exit_code: Some(0),
            at: "2026-10-03T12:00:00Z".into(),
        };
        std::fs::write(
            spool.path().join(format!("{field}.json")),
            serde_json::to_vec(&receipt).unwrap(),
        )
        .unwrap();
        assert_eq!(
            crate::recorder_receipts::ingest_once(&store, spool.path()).unwrap(),
            1
        );
        let subject = format!("{repository}/{segment}/8");
        let recorded = store.latest_actual_value(&subject).unwrap().unwrap()["facts"].clone();
        assert!(recorded.get("state").is_none());
        let previous = json!({"repository_id": 1, field: []});
        let current = json!({"repository_id": 1, field: [{
            "number": 8, "state": "open", "draft": false,
            "head": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        }]});
        let items = discovered_collection_items(
            repository, field, Some(&previous), &current,
            &mut |_| Some(recorded.clone()),
        );
        assert_eq!(items.len(), 1);
        assert!(items[0].deliver, "{field} must deliver its first observed state");
        assert_eq!(items[0].facts["opened_by"], "agent/node.builder");
        assert_eq!(items[0].facts["opened_by_run"], "mission-run/created");
        let observed = items[0].facts.clone();
        assert!(
            discovered_collection_items(
                repository, field, Some(&current), &current,
                &mut |_| Some(observed.clone()),
            )
            .is_empty(),
            "the same sighting must not deliver twice"
        );
        assert!(
            !discovered_collection_items(
                repository, field, None, &current,
                &mut |_| Some(recorded.clone()),
            )[0].deliver,
            "a repository baseline must not deliver receipt-born items"
        );
        let mut old_item = current.clone();
        old_item[field][0]["new"] = Value::Bool(false);
        assert!(
            !discovered_collection_items(
                repository, field, Some(&previous), &old_item,
                &mut |_| Some(recorded.clone()),
            )[0].deliver,
            "an explicitly historical item must not deliver"
        );
    }
}

#[test]
fn relocated_items_keep_subjects_and_resolution_through_partial_observations() {
    let repository = "resource/github/acme/garden";
    let previous = json!({"repository_id": 7});
    let current = json!({"repository_id": 7, "issues": [{
        "number": 1, "node_id": "I_orchid", "state": "closed", "new": false,
        "moved_to": "resource/github/acme/greenhouse/issue/77",
        "closed_by": "fern", "closed_by_resource": "resource/github/acme/greenhouse/pull-request/2",
        "state_reason": "completed"
    }]});
    let old =
        json!({"repository": repository, "number": 1, "node_id": "I_orchid", "state": "open"});
    let changed = discovered_collection_items(
        repository,
        "issues",
        Some(&previous),
        &current,
        &mut |_| Some(old.clone()),
    );
    assert_eq!(changed.len(), 1);
    assert_eq!(changed[0].subject, "resource/github/acme/garden/issue/1");
    assert!(!changed[0].deliver);
    assert_eq!(changed[0].facts["repository"], repository);
    assert_eq!(changed[0].facts["number"], 1);
    assert_eq!(
        changed[0].facts["moved_to"],
        "resource/github/acme/greenhouse/issue/77"
    );
    let partial = Value::Object(item_facts(
        repository,
        "issues",
        1,
        Some(&changed[0].facts),
        &json!({"number": 1, "comments": 4}),
    ));
    for name in [
        "node_id",
        "moved_to",
        "closed_by",
        "closed_by_resource",
        "state_reason",
    ] {
        assert_eq!(partial[name], changed[0].facts[name]);
    }
    assert!(
        discovered_collection_items(
            repository,
            "issues",
            Some(&previous),
            &current,
            &mut |_| Some(changed[0].facts.clone())
        )
        .is_empty()
    );
}

#[test]
fn repository_collections_create_one_typed_resource_for_each_new_item() {
    let previous = json!({"pull_requests": [], "issues": []});
    let current = json!({
        "pull_requests": [{
            "number": 7, "url": "https://example.test/pull/7", "title": "Ready",
            "head": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        }],
        "issues": [{"number": 8, "url": "https://example.test/issues/8", "title": "Bug"}],
    });
    let pulls = discovered_collection_items(
        "resource/github/acme/demo",
        "pull_requests",
        Some(&previous),
        &current,
        &mut |_| None,
    );
    assert_eq!(pulls.len(), 1);
    assert_eq!(pulls[0].subject, "resource/github/acme/demo/pull-request/7");
    assert_eq!(pulls[0].kind, "vcs.pull-request");
    assert_eq!(pulls[0].facts["repository"], "resource/github/acme/demo");
    assert_eq!(pulls[0].facts["draft"], false);
    assert!(pulls[0].deliver);

    let issues = discovered_collection_items(
        "resource/github/acme/demo",
        "issues",
        Some(&previous),
        &current,
        &mut |_| None,
    );
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0].subject, "resource/github/acme/demo/issue/8");
    assert_eq!(issues[0].kind, "vcs.issue");
    assert_eq!(issues[0].facts["repository"], "resource/github/acme/demo");
    assert!(issues[0].deliver);
}

#[test]
fn repository_pull_head_changes_and_closure_emit_exact_item_claims() {
    let before = json!({"repository_id": 7, "pull_requests": [{
        "number": 4, "head": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "state": "open", "draft": false,
    }]});
    let after = json!({"repository_id": 7, "pull_requests": [{
        "number": 4, "head": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        "state": "open", "draft": false, "branch": "agent/example",
        "author": "example-login", "opened_by": "agent/example/builder",
        "opened_by_run": "mission-run/example",
    }]});
    let changes = discovered_collection_items(
        "resource/github/acme/demo",
        "pull_requests",
        Some(&before),
        &after,
        &mut |_| None,
    );
    assert_eq!(changes.len(), 1);
    assert_eq!(
        changes[0].subject,
        "resource/github/acme/demo/pull-request/4"
    );
    assert!(changes[0].deliver);
    assert_eq!(
        changes[0].facts["head_sha"],
        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
    );
    assert_eq!(changes[0].facts["branch"], "agent/example");
    assert_eq!(changes[0].facts["author"], "example-login");
    assert_eq!(changes[0].facts["opened_by"], "agent/example/builder");
    assert_eq!(changes[0].facts["opened_by_run"], "mission-run/example");
    assert_eq!(changes[0].facts["merged"], false);
    let facts = changes[0]
        .facts
        .as_object()
        .unwrap()
        .iter()
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    st3_schema::registry()
        .validate_resource_facts("vcs.pull-request", &facts)
        .unwrap();
    // An item that only an older build's repository facts knew is recorded once, without a
    // review. Once its resource holds the same facts, the same sighting records nothing.
    let recorded = changes[0].facts.clone();
    let materialized = discovered_collection_items(
        "resource/github/acme/demo",
        "pull_requests",
        Some(&after),
        &after,
        &mut |_| None,
    );
    assert_eq!(materialized.len(), 1);
    assert!(!materialized[0].deliver);
    assert!(materialized[0].changed_fields.is_empty());
    assert!(
        discovered_collection_items(
            "resource/github/acme/demo",
            "pull_requests",
            Some(&json!({"repository_id": 7})),
            &after,
            &mut |_| Some(recorded.clone()),
        )
        .is_empty()
    );
    let closed = json!({"repository_id": 7, "pull_requests": [{
        "number": 4, "head": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        "state": "closed", "draft": false,
    }]});
    let changes = discovered_collection_items(
        "resource/github/acme/demo",
        "pull_requests",
        Some(&after),
        &closed,
        &mut |_| None,
    );
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].facts["state"], "closed");
    assert!(!changes[0].deliver, "a closure never asks for a review");
    assert!(
        changes[0].facts.get("merged").is_none(),
        "an open listing cannot tell a merge from a closure"
    );
}

#[test]
fn only_a_new_ready_head_of_an_open_pull_request_asks_for_review() {
    let a = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let b = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    let asks = |before: Value, after: Value| {
        let previous = json!({"repository_id": 7, "pull_requests": [before]});
        let current = json!({"repository_id": 7, "pull_requests": [after]});
        let changes = discovered_collection_items(
            "resource/github/acme/demo",
            "pull_requests",
            Some(&previous),
            &current,
            &mut |_| None,
        );
        assert!(changes.len() <= 1);
        changes.first().map(|change| change.deliver)
    };
    // An older build recorded no per-item state. Its open items gain one, and the items it
    // retained after they left the open listing are now closed. Neither is a new head.
    assert_eq!(
        asks(
            json!({"number": 4, "head": a}),
            json!({"number": 4, "head": a, "state": "open", "draft": false})
        ),
        Some(false)
    );
    assert_eq!(
        asks(
            json!({"number": 4, "head": a}),
            json!({"number": 4, "head": a, "state": "closed", "draft": false})
        ),
        Some(false)
    );
    // A pull request that closed and moved on is not reviewed either.
    assert_eq!(
        asks(
            json!({"number": 4, "head": a, "state": "open"}),
            json!({"number": 4, "head": b, "state": "closed", "draft": false})
        ),
        Some(false)
    );
    // Reopening at the same head, or a head first recorded for a known item, is a baseline.
    assert_eq!(
        asks(
            json!({"number": 4, "head": a, "state": "closed", "draft": false}),
            json!({"number": 4, "head": a, "state": "open", "draft": false})
        ),
        Some(false)
    );
    assert_eq!(
        asks(
            json!({"number": 4, "state": "open"}),
            json!({"number": 4, "head": a, "state": "open", "draft": false})
        ),
        Some(false)
    );
    // A draft is not reviewed, and it is when it becomes ready at the same head.
    assert_eq!(
        asks(
            json!({"number": 4, "head": a, "state": "open", "draft": false}),
            json!({"number": 4, "head": b, "state": "open", "draft": true})
        ),
        Some(false)
    );
    assert_eq!(
        asks(
            json!({"number": 4, "head": a, "state": "open", "draft": true}),
            json!({"number": 4, "head": a, "state": "open", "draft": false})
        ),
        Some(true)
    );
    // A new head of an open pull request is reviewed, including one that an older build
    // recorded without a state.
    assert_eq!(
        asks(
            json!({"number": 4, "head": a}),
            json!({"number": 4, "head": b, "state": "open", "draft": false})
        ),
        Some(true)
    );
    assert_eq!(
        asks(
            json!({"number": 3, "head": a}),
            json!({"number": 4, "head": b, "state": "open", "draft": false})
        ),
        Some(true)
    );
    // An open pull request with no head cannot be reviewed at a head.
    assert_eq!(
        asks(
            json!({"number": 3, "head": a}),
            json!({"number": 4, "state": "open", "draft": false})
        ),
        Some(false)
    );
}

#[test]
fn resource_observations_establish_a_baseline_and_send_selected_changes_once() {
    let store = Store::open_memory("node").unwrap();
    let source = r#"
version 2

  agent "one" { workspace "."; command "true" }
  agent "two" { workspace "."; command "true" }
  resource "github/acme/demo/pull/1" { kind "vcs.pull-request" }
  observer "github/acme/demo/pull/1" {
    resource "resource/github/acme/demo/pull/1"
    provider "github.pull-request"
    locator "acme/demo#1"
    field "head"
    field "state"
    field "checks"
  }
  subscription "one" {
    observer "observer/github/acme/demo/pull/1"
    to "agent/node.one"
    on "state"
    delivery "message"
  }
  subscription "two" {
    observer "observer/github/acme/demo/pull/1"
    to "agent/node.two"
    on "state"
    delivery "message"
  }
  subscription "missing" {
    observer "observer/github/acme/demo/pull/1"
    to "agent/node.missing"
    on "state"
    delivery "message"
  }

"#;
    let intent = parse_intent(source, "node").unwrap();
    let planned = store
        .mission(
            &intent,
            IntentInput {
                kdl: source.into(),
                source_name: None,
            },
        )
        .unwrap();
    assert!(planned.blockers.is_empty());
    assert!(
        planned
            .warnings
            .iter()
            .any(|warning| warning.contains("agent/node.missing"))
    );
    store
        .apply(&intent, &planned.subject_tokens, "publish-watch")
        .unwrap();
    let subscriptions = store
        .desired_subjects()
        .unwrap()
        .into_iter()
        .filter(|desired| desired.kind == "subscription")
        .map(|desired| {
            let spec = crate::graph::subscription_spec(&desired.desired).unwrap();
            (desired.subject, spec)
        })
        .collect::<Vec<_>>();
    let observer_revision = store
        .selected_desired_revision("observer/github/acme/demo/pull/1")
        .unwrap()
        .unwrap();
    assert_eq!(
        store
            .record_resource_observation(
                "observer/github/acme/demo/pull/1",
                "stale-revision",
                None,
                "resource/github/acme/demo/pull/1",
                Some("stale-cursor"),
                &json!({"head": "stale"}),
                50,
                &subscriptions,
                None,
            )
            .unwrap_err()
            .code,
        "stale-observer-revision"
    );
    let baseline = store.record_resource_observation("observer/github/acme/demo/pull/1", &observer_revision, None, "resource/github/acme/demo/pull/1", Some("cursor-one"), &json!({
        "head": "resource/github/acme/demo/ref/a",
        "state": "open",
        "checks": ["pending"]
    }), 100, &subscriptions, None)
        .unwrap();
    assert!(baseline.baseline);
    assert!(baseline.message_subjects.is_empty());
    assert_eq!(
        store
            .claims_for(
                "resource/github/acme/demo/pull/1",
                Some("resource.observed")
            )
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        store
            .latest_actual_value("subscription/missing")
            .unwrap()
            .unwrap()["state"],
        "pending"
    );
    let unchanged = store.record_resource_observation("observer/github/acme/demo/pull/1", &observer_revision, None, "resource/github/acme/demo/pull/1", Some("cursor-one"), &json!({
        "head": "resource/github/acme/demo/ref/a",
        "state": "open",
        "checks": ["pending"]
    }), 200, &subscriptions, None)
        .unwrap();
    assert!(unchanged.changed_fields.is_empty());
    assert!(unchanged.observation_claim.is_none());
    let changed = store.record_resource_observation("observer/github/acme/demo/pull/1", &observer_revision, None, "resource/github/acme/demo/pull/1", Some("cursor-two"), &json!({
        "head": "resource/github/acme/demo/ref/a",
        "state": "closed",
        "checks": ["pending"]
    }), 300, &subscriptions, None)
        .unwrap();
    assert_eq!(changed.changed_fields, ["state"]);
    assert_eq!(changed.message_subjects.len(), 2);
    let retry = store.record_resource_observation("observer/github/acme/demo/pull/1", &observer_revision, None, "resource/github/acme/demo/pull/1", Some("cursor-two"), &json!({
        "head": "resource/github/acme/demo/ref/a",
        "state": "closed",
        "checks": ["pending"]
    }), 300, &subscriptions, None)
        .unwrap();
    assert_eq!(retry.message_subjects, changed.message_subjects);
    assert_eq!(store.messages(None, true).unwrap().len(), 2);
    let stop_source = "version 2\n subscription \"one\" { stop } \n";
    let stop_intent = parse_intent(stop_source, "node").unwrap();
    let stop_mission = store
        .mission(
            &stop_intent,
            IntentInput {
                kdl: stop_source.into(),
                source_name: None,
            },
        )
        .unwrap();
    store
        .apply(
            &stop_intent,
            &stop_mission.subject_tokens,
            "stop-one-subscription",
        )
        .unwrap();
    let after_stop = store.record_resource_observation("observer/github/acme/demo/pull/1", &observer_revision, None, "resource/github/acme/demo/pull/1", Some("cursor-after-stop"), &json!({
        "head": "resource/github/acme/demo/ref/a",
        "state": "open",
        "checks": ["pending"]
    }), 350, &subscriptions, None)
        .unwrap();
    assert_eq!(after_stop.message_subjects.len(), 1);
    assert_eq!(store.messages(None, true).unwrap().len(), 3);
    let unselected = store.record_resource_observation("observer/github/acme/demo/pull/1", &observer_revision, None, "resource/github/acme/demo/pull/1", Some("cursor-three"), &json!({
        "head": "resource/github/acme/demo/ref/a",
        "state": "open",
        "checks": ["passing"]
    }), 400, &subscriptions, None)
        .unwrap();
    assert_eq!(unselected.changed_fields, ["checks"]);
    assert!(unselected.message_subjects.is_empty());
}

#[test]
fn a_subscription_condition_delivers_only_when_it_becomes_true() {
    let store = Store::open_memory("node").unwrap();
    let source = r#"
version 2

agent "target" { workspace "."; command "true" }
resource "pull" { kind "vcs.pull-request" }
observer "pull" {
  resource "resource/pull"
  provider "github.pull-request"
  locator "acme/demo#1"
  field "checks"
}
subscription "green" {
  observer "observer/pull"
  to "agent/node.target"
  on "checks"
  when {
    every "checks" {
      field "status" "is" "completed"
      field "conclusion" "is" "success"
    }
  }
  delivery "message"
}
"#;
    let intent = parse_intent(source, "node").unwrap();
    let planned = store
        .mission(
            &intent,
            IntentInput {
                kdl: source.into(),
                source_name: None,
            },
        )
        .unwrap();
    assert!(planned.blockers.is_empty());
    store
        .apply(&intent, &planned.subject_tokens, "publish-green-watch")
        .unwrap();
    let subscriptions = store
        .desired_subjects()
        .unwrap()
        .into_iter()
        .filter(|desired| desired.kind == "subscription")
        .map(|desired| {
            let spec = crate::graph::subscription_spec(&desired.desired).unwrap();
            (desired.subject, spec)
        })
        .collect::<Vec<_>>();
    let observer_revision = store
        .selected_desired_revision("observer/pull")
        .unwrap()
        .unwrap();
    let observe = |cursor: &str, checks: Value| {
        store.record_resource_observation("observer/pull", &observer_revision, None, "resource/pull", Some(cursor), &json!({"checks": checks}), 100, &subscriptions, None)
            .unwrap()
    };

    let baseline = observe(
        "baseline",
        json!([
            {"status": "completed", "conclusion": "success"},
            {"status": "in_progress", "conclusion": null}
        ]),
    );
    assert!(baseline.baseline);
    assert!(baseline.message_subjects.is_empty());

    let still_pending = observe(
        "still-pending",
        json!([
            {"status": "completed", "conclusion": "success", "completed_at": "one"},
            {"status": "in_progress", "conclusion": null}
        ]),
    );
    assert!(still_pending.message_subjects.is_empty());

    let green = observe(
        "green",
        json!([
            {"status": "completed", "conclusion": "success", "completed_at": "one"},
            {"status": "completed", "conclusion": "success", "completed_at": "two"}
        ]),
    );
    assert_eq!(green.message_subjects.len(), 1);

    let still_green = observe(
        "still-green",
        json!([
            {"status": "completed", "conclusion": "success", "completed_at": "updated"},
            {"status": "completed", "conclusion": "success", "completed_at": "two"}
        ]),
    );
    assert!(still_green.message_subjects.is_empty());

    let red = observe(
        "red",
        json!([
            {"status": "completed", "conclusion": "failure"},
            {"status": "completed", "conclusion": "success"}
        ]),
    );
    assert!(red.message_subjects.is_empty());
    let green_again = observe(
        "green-again",
        json!([
            {"status": "completed", "conclusion": "success"},
            {"status": "completed", "conclusion": "success"}
        ]),
    );
    assert_eq!(green_again.message_subjects.len(), 1);
    assert_ne!(green_again.message_subjects, green.message_subjects);
    assert_eq!(store.messages(None, true).unwrap().len(), 2);
}

#[test]
fn a_graph_wide_idempotency_key_returns_the_original_claim() {
    let store = Store::open_memory("node").unwrap();
    let request = ClaimInput {
        subject: "custom/test/fact".into(),
        kind: "custom.test.observed".into(),
        actor: Some("person/tester".into()),
        fields: BTreeMap::from([("value".into(), json!(1))]),
        evidence: Vec::new(),
        expected_subject: None,
        idempotency_key: Some("one-operation".into()),
    };
    let first = store.append_claim(&request).unwrap();
    let retry = store.append_claim(&request).unwrap();
    assert_eq!(retry.id, first.id);
    assert_eq!(retry.operation_id, first.operation_id);
    assert_eq!(store.claims_for(&request.subject, None).unwrap().len(), 1);

    let mut changed = request.clone();
    changed.fields.insert("value".into(), json!(2));
    let error = store.append_claim(&changed).unwrap_err();
    assert_eq!(error.code, "idempotency-mismatch");
}

#[test]
fn matching_replica_operations_converge_without_a_second_event() {
    let left = Store::open_memory("left").unwrap();
    let right = Store::open_memory("right").unwrap();
    let request = ClaimInput {
        subject: "custom/test/fact".into(),
        kind: "custom.test.observed".into(),
        actor: None,
        fields: BTreeMap::from([("value".into(), json!(1))]),
        evidence: Vec::new(),
        expected_subject: None,
        idempotency_key: Some("replicated-operation".into()),
    };
    let left_claim = left.append_claim(&request).unwrap();
    let right_claim = right.append_claim(&request).unwrap();
    assert_ne!(left_claim.id, right_claim.id);
    assert_eq!(left_claim.operation_id, right_claim.operation_id);

    left.import_replication("right", &right.export_replication(0).unwrap())
        .unwrap();
    let retry = left.append_claim(&request).unwrap();
    assert_eq!(
        retry.id,
        std::cmp::min(left_claim.id.clone(), right_claim.id.clone())
    );
    assert_eq!(left.events_after(0, None).unwrap().len(), 1);
}

#[test]
fn conflicting_replica_operations_make_the_subject_indeterminate() {
    let left = Store::open_memory("left").unwrap();
    let right = Store::open_memory("right").unwrap();
    let request = |value| ClaimInput {
        subject: "custom/test/fact".into(),
        kind: "custom.test.observed".into(),
        actor: None,
        fields: BTreeMap::from([("value".into(), json!(value))]),
        evidence: Vec::new(),
        expected_subject: None,
        idempotency_key: Some("conflicting-operation".into()),
    };
    left.append_claim(&request(1)).unwrap();
    right.append_claim(&request(2)).unwrap();
    left.import_replication("right", &right.export_replication(0).unwrap())
        .unwrap();
    assert!(left.operation_projection_drift().unwrap().is_empty());

    let error = left.append_claim(&request(1)).unwrap_err();
    assert_eq!(error.code, "idempotency-conflict");
    let status = left.status(Some("custom/test/fact")).unwrap();
    assert_eq!(status.subjects[0].reachability, "indeterminate");
    assert!(
        status.subjects[0]
            .reason
            .as_deref()
            .is_some_and(|value| value.contains("idempotency-conflict"))
    );
}

#[test]
fn public_claims_cannot_impersonate_system_producers() {
    let store = Store::open_memory("node").unwrap();
    let error = store
        .append_client_claim(&ClaimInput {
            subject: "host/peer".into(),
            kind: "transport.observed".into(),
            actor: None,
            fields: BTreeMap::from([("status".into(), Value::String("up".into()))]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap_err();
    assert_eq!(error.code, "claim-write-forbidden");
}

#[test]
fn an_agent_can_change_its_account_without_account_existence_coupling() {
    let store = Store::open_memory("node").unwrap();
    let association = |account: &str, actor: &str| ClaimInput {
        subject: "agent/run/worker".into(),
        kind: "agent.account".into(),
        actor: Some(actor.into()),
        fields: BTreeMap::from([("account".into(), Value::String(account.into()))]),
        evidence: Vec::new(),
        expected_subject: None,
        idempotency_key: None,
    };

    store
        .append_client_claim(&association(
            "account/claude/not-declared",
            "agent/run/worker",
        ))
        .unwrap();
    store
        .append_client_claim(&association("account/claude/team-a", "agent/run/worker"))
        .unwrap();
    assert_eq!(
        store
            .latest_actual_value("agent/run/worker")
            .unwrap()
            .unwrap()["account"],
        "account/claude/team-a"
    );
    assert_eq!(
        store
            .append_client_claim(&association("account/claude/team-b", "agent/run/other",))
            .unwrap_err()
            .code,
        "claim-write-forbidden"
    );
}

#[test]
fn once_cardinality_rejects_a_second_local_claim() {
    let store = Store::open_memory("node").unwrap();
    let request = ClaimInput {
        subject: "mission-run/one".into(),
        kind: "mission-run.created".into(),
        actor: None,
        fields: BTreeMap::new(),
        evidence: Vec::new(),
        expected_subject: None,
        idempotency_key: None,
    };
    store.append_claim(&request).unwrap();
    assert_eq!(
        store.append_claim(&request).unwrap_err().code,
        "claim-cardinality"
    );
}

#[test]
fn replicated_once_claims_preserve_concurrent_history() {
    let left = Store::open_memory("left").unwrap();
    let right = Store::open_memory("right").unwrap();
    for (store, verdict) in [(&left, "pass"), (&right, "fail")] {
        store
            .append_claim(&ClaimInput {
                subject: "mission-run/shared-eval".into(),
                kind: "eval.verdict".into(),
                actor: None,
                fields: BTreeMap::from([("verdict".into(), Value::String(verdict.into()))]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
    }

    let target = Store::open_memory("target").unwrap();
    target
        .import_replication("left", &left.export_replication(0).unwrap())
        .unwrap();
    target
        .import_replication("right", &right.export_replication(0).unwrap())
        .unwrap();
    assert_eq!(
        target
            .claims_for("mission-run/shared-eval", Some("eval.verdict"))
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn issue_openers_survive_publishers_and_direct_and_collection_observers() {
    for attribution in [
        json!({"opened_by_run": "mission-run/author"}),
        json!({"opened_by": "agent/node.author", "opened_by_run": "mission-run/author"}),
    ] {
        let store = Store::open_memory("node").unwrap();
        let source = r#"version 2
resource "github/acme/demo" { kind "vcs.repository" }
resource "github/acme/demo/issue/8" { kind "vcs.issue" }
observer "repo" {
  resource "resource/github/acme/demo"
  provider "github.repository"
  locator "acme/demo"
  field "issues"
}
observer "issue" {
  resource "resource/github/acme/demo/issue/8"
  provider "github.issue"
  locator "acme/demo#8"
  field "state"
}
"#;
        let intent = parse_intent(source, "node").unwrap();
        let planned = store
            .mission(&intent, IntentInput { kdl: source.into(), source_name: None })
            .unwrap();
        store.apply(&intent, &planned.subject_tokens, "publish-watch").unwrap();
        let subject = "resource/github/acme/demo/issue/8";
        let publish = |facts: Value| ClaimInput {
            subject: subject.into(),
            kind: "resource.observed".into(),
            actor: Some("agent/node.author".into()),
            fields: BTreeMap::from([
                ("kind".into(), Value::String("vcs.issue".into())),
                ("facts".into(), facts),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        };
        store.append_client_claim(&publish(attribution.clone())).unwrap();
        store.append_client_claim(&publish(json!({
            "opened_by_run": "mission-run/fixer", "state": "open"
        }))).unwrap();
        let observe = |observer: &str, resource: &str, facts: Value| {
            let revision = store.selected_desired_revision(observer).unwrap().unwrap();
            store.record_resource_observation(observer, &revision, None, resource, None, &facts, 50, &[], None).unwrap();
        };
        observe("observer/issue", subject, json!({"state": "closed"}));
        let actual = store.latest_actual_value(subject).unwrap().unwrap();
        assert_eq!(actual["facts"]["state"], "closed");
        assert_eq!(actual["facts"]["opened_by_run"], "mission-run/author");
        if let Some(opener) = attribution.get("opened_by") {
            assert_eq!(&actual["facts"]["opened_by"], opener);
        }
        observe("observer/repo", "resource/github/acme/demo", json!({"issues": []}));
        observe("observer/repo", "resource/github/acme/demo", json!({
            "issues": [{"number": 8, "state": "open", "title": "Updated issue"}]
        }));
        let actual = store.latest_actual_value(subject).unwrap().unwrap();
        assert_eq!(actual["facts"]["state"], "open");
        assert_eq!(actual["facts"]["title"], "Updated issue");
        assert_eq!(actual["facts"]["opened_by_run"], "mission-run/author");
        if let Some(opener) = attribution.get("opened_by") {
            assert_eq!(&actual["facts"]["opened_by"], opener);
            let changed = store.append_client_claim(&publish(json!({
                "opened_by": "agent/node.fixer", "opened_by_run": "mission-run/fixer"
            }))).unwrap();
            assert_eq!(&changed.body["fields"]["facts"]["opened_by"], opener);
            assert_eq!(changed.body["fields"]["facts"]["opened_by_run"], "mission-run/author");
        }
    }
}

#[test]
fn pull_request_openers_survive_partial_publishers_and_observers() {
    for attribution in [
        json!({"opened_by_run": "mission-run/author"}),
        json!({"opened_by": "agent/node.author", "opened_by_run": "mission-run/author"}),
    ] {
        let store = Store::open_memory("node").unwrap();
        let source = r#"version 2
resource "github/acme/demo/pull/9" { kind "vcs.pull-request" }
observer "pull" {
  resource "resource/github/acme/demo/pull/9"
  provider "github.pull-request"
  locator "acme/demo#9"
  field "state"
}
"#;
        let intent = parse_intent(source, "node").unwrap();
        let planned = store
            .mission(
                &intent,
                IntentInput {
                    kdl: source.into(),
                    source_name: None,
                },
            )
            .unwrap();
        store
            .apply(&intent, &planned.subject_tokens, "publish-watch")
            .unwrap();
        let subject = "resource/github/acme/demo/pull/9";
        let publish = |facts: Value| ClaimInput {
            subject: subject.into(),
            kind: "resource.observed".into(),
            actor: Some("agent/node.author".into()),
            fields: BTreeMap::from([
                ("kind".into(), Value::String("vcs.pull-request".into())),
                ("facts".into(), facts),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        };
        let mut opened = attribution.clone();
        opened["state"] = json!("open");
        store.append_client_claim(&publish(opened)).unwrap();
        // A publisher's partial snapshot of the merge keeps both fields.
        let merged = store
            .append_client_claim(&publish(json!({"state": "merged", "merged": true})))
            .unwrap();
        assert_eq!(merged.body["fields"]["facts"]["state"], "merged");
        assert_eq!(
            merged.body["fields"]["facts"]["opened_by_run"],
            "mission-run/author"
        );
        assert_eq!(
            merged.body["fields"]["facts"].get("opened_by"),
            attribution.get("opened_by")
        );
        // So does an observer that never reads them.
        let revision = store
            .selected_desired_revision("observer/pull")
            .unwrap()
            .unwrap();
        store.record_resource_observation("observer/pull", &revision, None, subject, None, &json!({"state": "closed"}), 50, &[], None)
            .unwrap();
        let actual = store.latest_actual_value(subject).unwrap().unwrap();
        assert_eq!(actual["facts"]["state"], "closed");
        assert_eq!(actual["facts"]["opened_by_run"], "mission-run/author");
        assert_eq!(
            actual["facts"].get("opened_by"),
            attribution.get("opened_by")
        );
        // A later claim cannot take the opener over.
        let fixer = store
            .append_client_claim(&publish(json!({
                "opened_by": "agent/node.fixer", "opened_by_run": "mission-run/fixer"
            })))
            .unwrap();
        assert_eq!(
            fixer.body["fields"]["facts"]["opened_by_run"],
            "mission-run/author"
        );
        if let Some(opener) = attribution.get("opened_by") {
            assert_eq!(&fixer.body["fields"]["facts"]["opened_by"], opener);
        }
    }
}

#[test]
fn resource_kind_and_immutable_facts_are_enforced() {
    let store = Store::open_memory("node").unwrap();
    let observed = |fields| ClaimInput {
        subject: "resource/commit".into(),
        kind: "resource.observed".into(),
        actor: None,
        fields,
        evidence: Vec::new(),
        expected_subject: None,
        idempotency_key: None,
    };
    store
        .append_client_claim(&observed(BTreeMap::from([
            ("kind".into(), Value::String("vcs.commit".into())),
            ("sha".into(), Value::String("abc".into())),
            ("state".into(), Value::String("published".into())),
        ])))
        .unwrap();
    let second = store
        .append_client_claim(&observed(BTreeMap::from([(
            "state".into(),
            Value::String("verified".into()),
        )])))
        .unwrap();
    assert_eq!(second.body["fields"]["kind"], "vcs.commit");
    assert_eq!(
        store
            .append_client_claim(&observed(BTreeMap::from([(
                "sha".into(),
                Value::String("def".into()),
            )])))
            .unwrap_err()
            .code,
        "immutable-resource-field"
    );
    assert_eq!(
        store
            .append_client_claim(&observed(BTreeMap::from([(
                "kind".into(),
                Value::String("ci.run".into()),
            )])))
            .unwrap_err()
            .code,
        "immutable-resource-kind"
    );
}

#[test]
fn queue_metadata_survives_runtime_views_and_reordering_resets_moved_work() {
    let store = Store::open_memory("node").unwrap();
    let publish = |order: &[&str], key: &str| {
        let items = order
            .iter()
            .map(|id| format!("step {id:?} {{ goal \"Complete {id}.\" }}"))
            .collect::<Vec<_>>()
            .join("\n");
        let source = format!(
            r#"version 2
mission "queue-revision" state="ready" {{
  goal "Process the ordered queue."
  queue "investigations" {{
    agentless
    {items}
  }}
}}"#
        );
        publish_mission(&store, &source, key)
    };

    let initial = publish(&["one", "two", "three"], "queue-revision-one");
    let run = store
        .create_mission_run(&MissionRunRequest {
            mission: initial.id,
            revision: None,
            workspace: ".".into(),
            requester: Some("person/test".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "queue-revision-run".into(),
        })
        .unwrap();
    let mut initial_steps = run
        .steps
        .iter()
        .map(|step| {
            (
                step.step.as_str(),
                step.queue.as_deref(),
                step.queue_position,
            )
        })
        .collect::<Vec<_>>();
    initial_steps.sort_by_key(|step| step.2);
    assert_eq!(
        initial_steps,
        vec![
            ("one", Some("investigations"), Some(1)),
            ("two", Some("investigations"), Some(2)),
            ("three", Some("investigations"), Some(3)),
        ]
    );
    for step in &run.steps {
        store
            .set_step_state(&step.subject, "completed", None)
            .unwrap();
        let direct = store.step_run(&step.subject).unwrap().unwrap();
        assert_eq!(direct.queue.as_deref(), Some("investigations"));
    }

    let reordered = publish(&["one", "three", "two"], "queue-revision-two");
    let revised = store
        .adopt_mission_revision(
            &run.id,
            &reordered,
            "person/test",
            "put the third investigation before the second",
            "queue-revision-cutover",
        )
        .unwrap();
    let mut revised_steps = revised
        .steps
        .iter()
        .map(|step| {
            (
                step.step.as_str(),
                step.status.as_str(),
                step.queue_position,
            )
        })
        .collect::<Vec<_>>();
    revised_steps.sort_by_key(|step| step.2);
    assert_eq!(
        revised_steps,
        vec![
            ("one", "completed", Some(1)),
            ("three", "pending", Some(2)),
            ("two", "pending", Some(3)),
        ]
    );
}

#[test]
fn pending_human_reviews_exclude_stale_and_terminal_owners() {
    let store = Store::open_memory("node").unwrap();
    let publish = |goal: &str, key: &str| {
        publish_mission(
            &store,
            &format!(
                r#"version 2
mission "review-current" state="ready" revision-cutover="restart-active" {{
  goal "Review current work."
  step "approval" {{ goal {goal:?}; agentless }}
}}"#
            ),
            key,
        )
    };
    let first = publish("Review the first revision.", "review-current-first");
    let run = store
        .create_mission_run(&MissionRunRequest {
            mission: first.id.clone(),
            revision: None,
            workspace: ".".into(),
            requester: Some("person/test".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "review-current-run".into(),
        })
        .unwrap();
    let first_step = run.steps[0].clone();
    let add_request = |subject: &str,
                       owner: &str,
                       revision: &str,
                       definition: &str,
                       attempt: u32,
                       key: &str| {
        store
            .append_claim(&ClaimInput {
                subject: subject.into(),
                kind: "gate.requested".into(),
                actor: None,
                fields: BTreeMap::from([
                    ("owner".into(), Value::String(owner.into())),
                    ("reviewer".into(), Value::String("person/alex".into())),
                    ("question".into(), Value::String("Approve it?".into())),
                    ("review_targets".into(), Value::Array(Vec::new())),
                    (
                        "decisions".into(),
                        Value::Array(vec![
                            Value::String("approved".into()),
                            Value::String("rejected".into()),
                        ]),
                    ),
                    ("operation".into(), Value::String(subject.into())),
                    ("mission_revision".into(), Value::String(revision.into())),
                    ("step_definition".into(), Value::String(definition.into())),
                    ("attempt".into(), Value::from(attempt)),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some(key.into()),
            })
            .unwrap()
    };
    add_request(
        "gate-operation/review-current/old-generation",
        &first_step.subject,
        &run.revision,
        &first_step.definition_hash,
        first_step.attempt,
        "old-generation-request",
    );
    assert_eq!(store.pending_human_reviews(None).unwrap().len(), 1);

    let second = publish("Review the second revision.", "review-current-second");
    let revised = store
        .adopt_mission_revision(
            &run.id,
            &second,
            "person/test",
            "use the new review step",
            "review-current-cutover",
        )
        .unwrap();
    assert!(store.pending_human_reviews(None).unwrap().is_empty());
    let current_step = revised.steps[0].clone();

    add_request(
        "gate-operation/review-current/bad-definition",
        &current_step.subject,
        &revised.revision,
        "old-definition",
        current_step.attempt,
        "bad-definition-request",
    );
    add_request(
        "gate-operation/review-current/bad-attempt",
        &current_step.subject,
        &revised.revision,
        &current_step.definition_hash,
        current_step.attempt + 1,
        "bad-attempt-request",
    );
    assert!(store.pending_human_reviews(None).unwrap().is_empty());

    add_request(
        "gate-operation/review-current/terminal-step",
        &current_step.subject,
        &revised.revision,
        &current_step.definition_hash,
        current_step.attempt,
        "terminal-step-request",
    );
    assert_eq!(store.pending_human_reviews(None).unwrap().len(), 1);
    store
        .set_step_state(&current_step.subject, "completed", None)
        .unwrap();
    assert!(store.pending_human_reviews(None).unwrap().is_empty());

    add_request(
        "gate-operation/review-current/terminal",
        &revised.subject,
        &revised.revision,
        &revised.revision,
        1,
        "terminal-request",
    );
    assert_eq!(store.pending_human_reviews(None).unwrap().len(), 1);
    store
        .set_mission_run_state(&revised.id, "cancelled", "normal", None)
        .unwrap();
    assert!(store.pending_human_reviews(None).unwrap().is_empty());
}

#[test]
fn a_gate_asked_again_is_one_review_that_has_waited_since_its_first_request() {
    let store = Store::open_memory("node").unwrap();
    publish_mission(
        &store,
        r#"version 2
mission "asked-again" state="ready" {
  goal "Review one change."
  step "approval" { goal "Approve the change."; agentless }
}"#,
        "asked-again-mission",
    );
    let run = store
        .create_mission_run(&MissionRunRequest {
            mission: "asked-again".into(),
            revision: None,
            workspace: ".".into(),
            requester: Some("person/test".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "asked-again-run".into(),
        })
        .unwrap();
    let step = run.steps[0].clone();
    // An older build asked without a mode; a newer build words the same gate with one,
    // so its request is a new operation.
    let ask = |operation: &str, mode: Option<&str>| {
        let mut fields = BTreeMap::from([
            ("owner".into(), Value::String(step.subject.clone())),
            ("reviewer".into(), Value::String("person/alex".into())),
            ("question".into(), Value::String("Approve it?".into())),
            ("review_targets".into(), Value::Array(Vec::new())),
            (
                "decisions".into(),
                Value::Array(vec![
                    Value::String("approved".into()),
                    Value::String("rejected".into()),
                ]),
            ),
            ("operation".into(), Value::String(operation.into())),
            (
                "mission_revision".into(),
                Value::String(run.revision.clone()),
            ),
            (
                "step_definition".into(),
                Value::String(step.definition_hash.clone()),
            ),
            ("attempt".into(), Value::from(step.attempt)),
        ]);
        if let Some(mode) = mode {
            fields.insert("mode".into(), Value::String(mode.into()));
        }
        store
            .append_claim(&ClaimInput {
                subject: operation.into(),
                kind: "gate.requested".into(),
                actor: None,
                fields,
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some(operation.into()),
            })
            .unwrap()
    };
    let first = ask("gate-operation/asked-again/first", None);
    std::thread::sleep(std::time::Duration::from_millis(2));
    let again = ask("gate-operation/asked-again/again", Some("approve"));
    assert!(again.accepted_at_unix_ms > first.accepted_at_unix_ms);

    let reviews = store.pending_human_reviews(None).unwrap();
    assert_eq!(reviews.len(), 1, "{reviews:?}");
    assert_eq!(
        reviews[0].request, again.id,
        "the reviewer answers the request the gate waits on"
    );
    assert_eq!(reviews[0].requested_at_unix_ms, first.accepted_at_unix_ms);

    let items = store.attention_items(Some("person/alex")).unwrap();
    let gates = items
        .iter()
        .filter(|item| item.kind == "human-gate")
        .collect::<Vec<_>>();
    assert_eq!(gates.len(), 1);
    assert_eq!(gates[0].requested_at_unix_ms, first.accepted_at_unix_ms);
}

fn request_fault(store: &Store, subject: &str, targets: &[&str]) {
    store
        .request_attention(
            subject,
            &AttentionRequest {
                reviewer: "person/alex".into(),
                title: format!("Fault {subject}"),
                reason: "a person must decide".into(),
                severity: "warning".into(),
                targets: targets.iter().map(|target| (*target).to_owned()).collect(),
                actor: "daemon/runtime".into(),
                idempotency_key: format!("{subject}:requested"),
            },
        )
        .unwrap();
}

fn start_agentless_run(store: &Store, mission: &str) -> MissionRunView {
    publish_mission(
        store,
        &format!(
            r#"version 2
mission "{mission}" state="ready" {{
  goal "Hold a gate."
  step "approval" {{ goal "Approve it."; agentless }}
}}"#
        ),
        &format!("{mission}-publish"),
    );
    store
        .create_mission_run(&MissionRunRequest {
            mission: mission.into(),
            revision: None,
            workspace: ".".into(),
            requester: Some("person/alex".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: format!("{mission}-run"),
        })
        .unwrap()
}

#[test]
fn attention_target_states_describe_each_target_with_a_lifecycle() {
    let store = Store::open_memory("node").unwrap();
    publish_mission(
        &store,
        r#"version 2
mission "typecase" state="ready" {
  goal "Publish a revision."
  step "publish" { goal "Publish it."; agentless }
}"#,
        "typecase-publish",
    );
    let run = store
        .create_mission_run(&MissionRunRequest {
            mission: "typecase".into(),
            revision: None,
            workspace: ".".into(),
            requester: Some("person/alex".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "typecase-run".into(),
        })
        .unwrap();
    let request = AttentionRequest {
        reviewer: "person/alex".into(),
        title: "Another fault".into(),
        reason: "a person must decide".into(),
        severity: "warning".into(),
        targets: Vec::new(),
        actor: "agent/node.requester".into(),
        idempotency_key: "other-fault".into(),
    };
    store
        .request_attention("attention/other-fault", &request)
        .unwrap();
    let targets = [
        "mission/typecase",
        run.subject.as_str(),
        "attention/other-fault",
        "resource/typecase/kdl",
        "agent/node.stopped",
    ]
    .map(str::to_owned);
    let states = |store: &Store| {
        store
            .attention_target_states(&targets)
            .unwrap()
            .into_iter()
            .map(|state| (state.id, state.state))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        states(&store),
        [
            ("mission/typecase".into(), "running".into()),
            (run.subject.clone(), "running".into()),
            ("attention/other-fault".into(), "open".into()),
        ]
    );

    store
        .set_mission_run_state(&run.id, "cancelled", "terminal", Some("moved to a seat"))
        .unwrap();
    store
        .withdraw_attention(
            "attention/other-fault",
            &AttentionWithdrawRequest {
                reason: "cleared".into(),
                actor: "agent/node.requester".into(),
                idempotency_key: "other-fault-withdrawn".into(),
            },
        )
        .unwrap();
    let source = "version 2\nstop \"agent/node.stopped\"\n";
    let intent = crate::graph::parse_intent(source, "node").unwrap();
    let planned = store
        .mission(
            &intent,
            IntentInput {
                kdl: source.into(),
                source_name: None,
            },
        )
        .unwrap();
    store
        .apply(&intent, &planned.subject_tokens, "stopped-agent")
        .unwrap();

    assert_eq!(
        states(&store),
        [
            ("mission/typecase".into(), "cancelled".into()),
            (run.subject.clone(), "cancelled".into()),
            ("attention/other-fault".into(), "withdrawn".into()),
            ("agent/node.stopped".into(), "stopped".into()),
        ]
    );
    assert!(
        store
            .attention_target_states(&targets)
            .unwrap()
            .iter()
            .all(|state| state.since_unix_ms.is_some())
    );
}

#[test]
fn explicit_attention_is_idempotent_authorized_and_terminal() {
    let store = Store::open_memory("node").unwrap();
    let request = AttentionRequest {
        reviewer: "alex".into(),
        title: "Fabric needs review".into(),
        reason: "The queue did not recover.".into(),
        severity: "error".into(),
        targets: vec!["resource/fabric/queue".into()],
        actor: "agent/fabric/worker".into(),
        idempotency_key: "attention-fabric-queue".into(),
    };
    let first = store
        .request_attention("attention/fabric-queue", &request)
        .unwrap();
    let retry = store
        .request_attention("attention/fabric-queue", &request)
        .unwrap();
    assert_eq!(first.request, retry.request);
    assert_eq!(first.reviewer, "person/alex");
    assert_eq!(first.status, "pending");
    let items = store.attention_items(Some("person/alex")).unwrap();
    assert!(
        items.is_empty(),
        "historical legacy records do not enter current attention"
    );
    assert!(
        store
            .attention_items(Some("person/someone-else"))
            .unwrap()
            .is_empty()
    );

    let unknown = AttentionResolveRequest {
        delegation: None,
        outcome: "resolved".into(),
        reason: None,
        actor: "client/unknown".into(),
        idempotency_key: "resolve-fabric-unknown".into(),
    };
    assert_eq!(
        store
            .resolve_attention(&first.subject, &unknown)
            .unwrap_err()
            .code,
        "invalid-attention-resolver"
    );
    // Any agent can close an item routed to a person, and is recorded as the resolver.
    let resolution = AttentionResolveRequest {
        delegation: None,
        outcome: "dismissed".into(),
        reason: Some("The fault is expected during maintenance.".into()),
        actor: "agent/fabric/other".into(),
        idempotency_key: "resolve-fabric".into(),
    };
    let closed = store
        .resolve_attention(&first.subject, &resolution)
        .unwrap();
    assert_eq!(closed.status, "dismissed");
    assert_eq!(
        store
            .latest_claim(&first.subject, Some("attention.resolved"))
            .unwrap()
            .unwrap()
            .actor
            .as_deref(),
        Some("agent/fabric/other")
    );
    assert_eq!(closed.outcome.as_deref(), Some("dismissed"));
    assert!(store.attention_items(None).unwrap().is_empty());
    let retry = store
        .resolve_attention(&first.subject, &resolution)
        .unwrap();
    assert_eq!(retry.resolved_at_unix_ms, closed.resolved_at_unix_ms);
}

#[test]
fn a_declared_person_message_stays_in_conversations_and_out_of_attention() {
    let store = Store::open_memory("node").unwrap();
    let source = r#"
version 2
message "human-attention" {
  from "agent/demo/worker"
  to "person/alex"
  title "Please review"
  content "The declarative message is ready."
}
"#;
    let intent = crate::graph::parse_test_intent(source, "node").unwrap();
    let preview = store
        .mission(
            &intent,
            IntentInput {
                kdl: source.into(),
                source_name: None,
            },
        )
        .unwrap();
    store
        .apply(&intent, &preview.subject_tokens, "desired-human-attention")
        .unwrap();

    let mailbox = store.messages(Some("person/alex"), false).unwrap();
    assert_eq!(mailbox.len(), 1);
    assert_eq!(mailbox[0].title.as_deref(), Some("Please review"));
    assert!(
        store
            .attention_items(Some("person/alex"))
            .unwrap()
            .is_empty()
    );
    assert!(store.attention_items(None).unwrap().is_empty());
}

#[test]
fn an_unread_person_message_never_enters_attention() {
    let store = Store::open_memory("node").unwrap();
    let message = store
        .append_claim(&ClaimInput {
            subject: "message/human-attention".into(),
            kind: "message.sent".into(),
            actor: Some("agent/demo/worker".into()),
            fields: BTreeMap::from([
                ("from".into(), Value::String("agent/demo/worker".into())),
                ("to".into(), Value::String("person/alex".into())),
                ("title".into(), Value::String("Can you look?".into())),
                ("content".into(), Value::String("Please read this.".into())),
                ("status".into(), Value::String("sent".into())),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some("human-attention-message".into()),
        })
        .unwrap();
    let quiet = |store: &Store| {
        store
            .attention_items(Some("person/alex"))
            .unwrap()
            .is_empty()
            && store.attention_items(None).unwrap().is_empty()
    };
    assert!(quiet(&store));

    store
        .append_claim(&ClaimInput {
            subject: message.subject.clone(),
            kind: "message.delivered".into(),
            actor: None,
            fields: BTreeMap::from([
                ("status".into(), Value::String("delivered".into())),
                ("recipient".into(), Value::String("person/alex".into())),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some("deliver-human-attention-message".into()),
        })
        .unwrap();
    assert!(quiet(&store));
    assert_eq!(
        store.messages(Some("person/alex"), false).unwrap()[0].status,
        "delivered"
    );
}

#[test]
fn login_attention_skips_healthy_seats_and_retains_legacy_and_runtime_fences() {
    let store = Store::open_memory("node").unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let names = ["native", "legacy", "screen", "stale", "healthy"];
    let source = format!(
        "version 2\n{}",
        names
            .iter()
            .map(|name| format!(
                "agent {name:?} {{ workspace {:?}; harness \"claude\" {{}} }}\n",
                workspace.path().display().to_string()
            ))
            .collect::<String>()
    );
    let intent = parse_intent(&source, "node").unwrap();
    let preview = store
        .mission(
            &intent,
            IntentInput {
                kdl: source,
                source_name: None,
            },
        )
        .unwrap();
    store
        .apply_as(
            &intent,
            &preview.subject_tokens,
            "login-candidates",
            Some("person/avery"),
        )
        .unwrap();
    let append = |name: &str, kind: &str, fields: Value| {
        store
            .append_claim(&ClaimInput {
                subject: format!("agent/node.{name}"),
                kind: kind.into(),
                actor: Some(format!("agent/node.{name}")),
                fields: serde_json::from_value(fields).unwrap(),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
    };
    for name in names {
        append(
            name,
            "runtime.observed",
            json!({"status":"running", "incarnation_id":"current"}),
        );
    }
    append(
        "native",
        "harness.observed",
        json!({"state":"idle", "incarnation_id":"current", "provider_auth":false}),
    );
    append(
        "legacy",
        "harness.observed",
        json!({"state":"blocked", "incarnation_id":"current", "reason":"providerAuth"}),
    );
    append(
        "screen",
        "harness.diagnostic",
        json!({"code":"provider-auth-expired", "incarnation_id":"current", "driver":"claude"}),
    );
    append(
        "stale",
        "harness.observed",
        json!({"state":"idle", "incarnation_id":"prior", "provider_auth":false}),
    );
    append(
        "healthy",
        "harness.observed",
        json!({"state":"idle", "incarnation_id":"current", "provider_auth":null}),
    );
    // These seats share the harness's default login on one host, so one alert names them all.
    let login_subjects = || {
        let logins = store
            .attention_items(Some("person/avery"))
            .unwrap()
            .into_iter()
            .filter(|item| item.kind == "harness-login")
            .collect::<Vec<_>>();
        assert!(logins.len() <= 1, "{logins:?}");
        logins
            .into_iter()
            .flat_map(|item| item.targets)
            .collect::<BTreeSet<_>>()
    };
    let (items, reads) = smallclaims::touched::record(login_subjects);
    assert_eq!(
        items,
        BTreeSet::from([
            "agent/node.native".into(),
            "agent/node.legacy".into(),
            "agent/node.screen".into(),
        ])
    );
    assert!(
        reads.contains("agent/node.stale"),
        "positive evidence still needs its runtime fence"
    );
    assert!(
        !reads.contains("agent/node.healthy"),
        "healthy seats must not rebuild their harness for attention"
    );
    assert!(reads.contains("kind:harness.observed"));
    assert!(reads.contains("kind:harness.diagnostic"));
    for name in ["native", "legacy"] {
        append(
            name,
            "harness.observed",
            json!({"state":"idle", "incarnation_id":"current", "provider_auth":true, "reason":null}),
        );
    }
    append(
        "screen",
        "harness.diagnostic",
        json!({"code":"provider-auth-restored", "incarnation_id":"current", "driver":"claude"}),
    );
    assert!(
        login_subjects().is_empty(),
        "retained negative evidence cannot defeat recovery"
    );
    append(
        "healthy",
        "harness.observed",
        json!({"state":"idle", "incarnation_id":"current", "provider_auth":false}),
    );
    assert_eq!(
        login_subjects(),
        BTreeSet::from(["agent/node.healthy".into()])
    );
}

#[test]
fn one_login_alert_per_login_directory_names_every_seat_that_shares_it() {
    let store = Store::open_memory("node").unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let workspace = workspace.path().display().to_string();
    let source = format!(
        r#"version 2
account "avery/one" {{ provider "anthropic"; owner "person/avery"; plan "max"; login "/logins/one"; }}
account "avery/two" {{ provider "anthropic"; owner "person/avery"; plan "max"; login "/logins/two"; }}
agent "first" {{ workspace {workspace:?}; harness "claude" {{ account "avery/one"; }} }}
agent "second" {{ workspace {workspace:?}; harness "claude" {{ account "avery/one"; }} }}
agent "third" {{ workspace {workspace:?}; harness "claude" {{ account "avery/two"; }} }}
"#
    );
    let intent = parse_intent(&source, "node").unwrap();
    let preview = store
        .mission(
            &intent,
            IntentInput {
                kdl: source,
                source_name: None,
            },
        )
        .unwrap();
    store
        .apply_as(&intent, &preview.subject_tokens, "logins", Some("person/avery"))
        .unwrap();
    for name in ["first", "second", "third"] {
        for (kind, fields) in [
            ("runtime.observed", json!({"status":"running", "incarnation_id":"current"})),
            ("harness.diagnostic", json!({"code":"provider-auth-expired", "incarnation_id":"current", "driver":"claude"})),
        ] {
            store
                .append_claim(&ClaimInput {
                    subject: format!("agent/node.{name}"),
                    kind: kind.into(),
                    actor: Some(format!("agent/node.{name}")),
                    fields: serde_json::from_value(fields).unwrap(),
                    evidence: vec![],
                    expected_subject: None,
                    idempotency_key: None,
                })
                .unwrap();
        }
    }
    let logins = store
        .attention_items(Some("person/avery"))
        .unwrap()
        .into_iter()
        .filter(|item| item.kind == "harness-login")
        .map(|item| (item.targets.clone(), (item.is_alert(), item.conversations())))
        .collect::<BTreeMap<_, _>>();
    let seats = |names: &[&str]| {
        names
            .iter()
            .map(|name| format!("agent/node.{name}"))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        logins,
        BTreeMap::from([
            (seats(&["first", "second"]), (true, seats(&["first", "second"]))),
            (seats(&["third"]), (true, seats(&["third"]))),
        ])
    );
}

/// A native prompt is an alert in its seat's conversation while the harness waits on it, and
/// clears when the harness reports it gone, when the terminal shows it was refused, or when
/// the seat restarts.
#[test]
fn a_native_prompt_is_an_alert_until_its_harness_or_terminal_says_it_is_gone() {
    let store = Store::open_memory("node").unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let source = format!(
        "version 2\nagent \"worker\" {{ workspace {:?}; harness \"claude\" {{}} }}\n",
        workspace.path().display().to_string()
    );
    let intent = parse_intent(&source, "node").unwrap();
    let preview = store
        .mission(
            &intent,
            IntentInput {
                kdl: source,
                source_name: None,
            },
        )
        .unwrap();
    store
        .apply_as(&intent, &preview.subject_tokens, "prompt-seat", Some("person/avery"))
        .unwrap();
    let append = |kind: &str, fields: Value| {
        store
            .append_claim(&ClaimInput {
                subject: "agent/node.worker".into(),
                kind: kind.into(),
                actor: Some("agent/node.worker".into()),
                fields: serde_json::from_value(fields).unwrap(),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap()
    };
    let prompts = || {
        store
            .attention_items(Some("person/avery"))
            .unwrap()
            .into_iter()
            .filter(|item| item.kind == "harness-prompt")
            .collect::<Vec<_>>()
    };
    append("runtime.observed", json!({"status":"running", "incarnation_id":"one"}));
    assert!(prompts().is_empty());
    let asked = append(
        "harness.observed",
        json!({"state":"working", "incarnation_id":"one", "blocked_on":"human",
            "ask":"permission", "reason":"Deploy production?"}),
    );
    let shown = prompts();
    assert_eq!(shown.len(), 1, "{shown:?}");
    assert_eq!(shown[0].episode, asked.id);
    assert!(shown[0].is_alert());
    assert_eq!(shown[0].conversation.as_deref(), Some("agent/node.worker"));
    assert!(shown[0].detail.starts_with("Deploy production?"));
    // Answered in the terminal: the harness reports it is no longer blocked.
    append(
        "harness.observed",
        json!({"state":"working", "incarnation_id":"one", "blocked_on":null, "ask":null}),
    );
    assert!(prompts().is_empty());
    // Asked again, then refused in the terminal, which only the screen shows.
    let again = append(
        "harness.observed",
        json!({"state":"working", "incarnation_id":"one", "blocked_on":"human", "ask":"question"}),
    );
    assert_eq!(prompts()[0].episode, again.id);
    store
        .append_claim(&ClaimInput {
            subject: "agent/node.worker".into(),
            kind: "harness.diagnostic".into(),
            actor: Some("agent/node.worker".into()),
            fields: serde_json::from_value(
                json!({"code":"native-prompt-gone", "incarnation_id":"one"}),
            )
            .unwrap(),
            evidence: vec![again.id.clone()],
            expected_subject: None,
            idempotency_key: Some(attention_snapshot::native_prompt_gone_key(&again.id)),
        })
        .unwrap();
    assert!(prompts().is_empty());
    // A prompt from an incarnation that is no longer running waits on nobody.
    append(
        "harness.observed",
        json!({"state":"working", "incarnation_id":"one", "blocked_on":"human", "ask":"permission"}),
    );
    assert_eq!(prompts().len(), 1);
    append("runtime.observed", json!({"status":"running", "incarnation_id":"two"}));
    assert!(prompts().is_empty());
}

#[test]
fn claude_native_prompt_lookup_window_never_infers_missing_episode_closure() {
    let store = Store::open_memory("node").unwrap();
    let seat = "agent/node.worker";
    let observe = |ownership: u64, transition: u64, state: &str, blocked: &str, ask: &str| {
        store.append_claim(&ClaimInput {
            subject: seat.into(),
            kind: "harness.observed".into(),
            actor: Some(seat.into()),
            fields: serde_json::from_value(json!({
                "driver":"claude", "state":state, "incarnation_id":"one",
                "blocked_on":blocked, "ask":ask,
                "ownership_sequence":ownership, "transition_sequence":transition,
            })).unwrap(),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: None,
        }).unwrap()
    };
    for _ in 0..20 {
        observe(1, 1, "idle", "none", "none");
    }
    // The new writer can precede its publication even with a full retained window.
    assert_eq!(store.native_prompt_state(seat, 1, 2).unwrap(), NativePromptState::Open);
    for _ in 0..20 {
        observe(1, 2, "indeterminate", "unknown", "unknown");
    }
    assert_eq!(store.native_prompt_state(seat, 1, 2).unwrap(), NativePromptState::Open);
    observe(1, 2, "working", "human", "permission");
    assert_eq!(store.native_prompt_state(seat, 1, 2).unwrap(), NativePromptState::Open);
    for _ in 0..20 {
        observe(1, 2, "working", "human", "permission");
    }
    assert_eq!(store.native_prompt_state(seat, 1, 2).unwrap(), NativePromptState::Open);
    // Only positive successor evidence releases a record outside the bounded window.
    for _ in 0..20 {
        observe(1, 3, "idle", "none", "none");
    }
    assert_eq!(store.native_prompt_state(seat, 1, 2).unwrap(), NativePromptState::Gone);
    assert_eq!(store.native_prompt_state(seat, 1, 4).unwrap(), NativePromptState::Open);
}

#[test]
fn claude_native_human_release_is_episode_fenced_durable_and_shared_with_sender_status() {
    for ask in ["permission", "question", "review"] {
        let store = Store::open_memory("node").unwrap();
        let seat = "agent/node.worker";
        let append = |kind: &str, fields: Value, key: Option<String>| {
            store.append_claim(&ClaimInput {
                subject: seat.into(), kind: kind.into(), actor: Some(seat.into()),
                fields: serde_json::from_value(fields).unwrap(), evidence: vec![],
                expected_subject: None, idempotency_key: key,
            }).unwrap()
        };
        append("runtime.observed", json!({"status":"running", "incarnation_id":"one"}), None);
        let blocked = |transition| json!({
            "state":"working", "driver":"claude", "incarnation_id":"one",
            "blocked_on":"human", "ask":ask, "reason":"native prompt",
            "ownership_sequence":1, "transition_sequence":transition
        });
        let asked = append("harness.observed", blocked(3), None);
        let before = store.index().unwrap();
        assert_eq!(store.native_prompt_state(seat, 1, 3).unwrap(), NativePromptState::Open, "{ask}");
        assert_eq!(store.observed_harness_at(seat, before).unwrap().unwrap().blocked_on.as_deref(), Some("human"));
        append("harness.diagnostic", json!({
            "code":"native-prompt-gone", "status":"resolved",
            "driver":"claude", "incarnation_id":"one"
        }), Some(native_prompt_gone_key(&asked.id)));
        assert_eq!(store.native_prompt_state(seat, 1, 3).unwrap(), NativePromptState::Gone, "{ask}");
        assert_eq!(store.observed_harness_at(seat, before).unwrap().unwrap().blocked_on.as_deref(), Some("human"));
        if ask == "permission" {
            // A replicated answer request can arrive after native closure.
            // It must not resurrect the sender's held projection.
            append("harness.diagnostic", json!({
                "code":"native-prompt-answered", "status":"allow",
                "driver":"claude", "incarnation_id":"one"
            }), Some(format!("native-prompt-answer:{}", asked.id)));
            assert_eq!(store.native_prompt_state(seat, 1, 3).unwrap(), NativePromptState::Gone);
            assert!(store.observed_harness_at(seat, store.index().unwrap()).unwrap().unwrap().blocked_on.is_none());
        }
        for _ in 0..20 {
            append("harness.observed", blocked(3), None);
        }
        // Esc/No emits no hook event; the blocked driver file can heartbeat
        // for longer than the bounded recent-observation lookup window.
        assert_eq!(store.native_prompt_state(seat, 1, 3).unwrap(), NativePromptState::Gone, "{ask}");
        let released = store.observed_harness_at(seat, store.index().unwrap()).unwrap().unwrap();
        assert!(released.blocked_on.is_none(), "{ask}");
        assert!(released.ask.is_none(), "{ask}");
        assert_eq!(released.state, "idle", "{ask}");
        // A full history window cannot release a not-yet-published new prompt.
        assert_eq!(store.native_prompt_state(seat, 1, 999).unwrap(), NativePromptState::Open);
        append("harness.observed", blocked(4), None);
        assert_eq!(store.native_prompt_state(seat, 1, 4).unwrap(), NativePromptState::Open, "{ask}");
        assert_eq!(store.observed_harness_at(seat, store.index().unwrap()).unwrap().unwrap().blocked_on.as_deref(), Some("human"));
        // Staleness is uncertainty, not prompt dismissal. Its derived
        // snapshot retains the driver's ownership/transition counters.
        append("harness.observed", json!({
            "state":"indeterminate", "driver":"claude", "incarnation_id":"one",
            "blocked_on":"unknown", "ask":"unknown", "reason":"stale",
            "ownership_sequence":1, "transition_sequence":4
        }), None);
        assert_eq!(store.native_prompt_state(seat, 1, 4).unwrap(), NativePromptState::Open, "{ask}");
        append("harness.observed", blocked(4), None);
        assert_eq!(store.native_prompt_state(seat, 1, 4).unwrap(), NativePromptState::Open, "{ask}");
        assert_eq!(store.observed_harness_at(seat, store.index().unwrap()).unwrap().unwrap().blocked_on.as_deref(), Some("human"));
        // A native timeout that ends the turn, unlike the hook's polling
        // timeout while a dialog stays open, retires the hold.
        append("harness.observed", json!({
            "state":"ended", "driver":"claude", "incarnation_id":"one",
            "ownership_sequence":1, "transition_sequence":5, "exit":"timeout"
        }), None);
        assert_eq!(store.native_prompt_state(seat, 1, 4).unwrap(), NativePromptState::Gone);
        assert!(store.observed_harness_at(seat, store.index().unwrap()).unwrap().unwrap().blocked_on.is_none());
        append("runtime.observed", json!({"status":"running", "incarnation_id":"two"}), None);
        append("harness.observed", json!({
            "state":"ready", "driver":"claude", "incarnation_id":"two",
            "ownership_sequence":2, "transition_sequence":1
        }), None);
        assert!(store.observed_harness_at(seat, store.index().unwrap()).unwrap().unwrap().blocked_on.is_none());
    }
}

/// A Claude permission prompt can be answered from a client: only by the seat's person, only
/// while it waits, and once. The hook sees the answer; the alert goes once it is answered.
#[test]
fn a_claude_permission_prompt_is_answered_once_by_its_person_while_it_waits() {
    let store = Store::open_memory("node").unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let source = format!(
        "version 2\nagent \"worker\" {{ workspace {:?}; harness \"claude\" {{}} }}\n",
        workspace.path().display().to_string()
    );
    let intent = parse_intent(&source, "node").unwrap();
    let preview = store
        .mission(
            &intent,
            IntentInput {
                kdl: source,
                source_name: None,
            },
        )
        .unwrap();
    store
        .apply_as(&intent, &preview.subject_tokens, "answer-seat", Some("person/avery"))
        .unwrap();
    let seat = "agent/node.worker";
    let append = |kind: &str, fields: Value| {
        store
            .append_claim(&ClaimInput {
                subject: seat.into(),
                kind: kind.into(),
                actor: Some(seat.into()),
                fields: serde_json::from_value(fields).unwrap(),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap()
    };
    // The hook waits on its own observation, named by its state record's sequences.
    assert_eq!(
        store.native_prompt_state(seat, 1, 3).unwrap(),
        NativePromptState::Open,
        "the hook's observation is not published yet"
    );
    append("runtime.observed", json!({"status":"running", "incarnation_id":"one"}));
    let asked = append(
        "harness.observed",
        json!({"state":"working", "driver":"claude", "incarnation_id":"one",
            "blocked_on":"human", "ask":"permission", "reason":"Bash",
            "ownership_sequence":1, "transition_sequence":3}),
    );
    let prompt = || {
        store
            .attention_items(Some("person/avery"))
            .unwrap()
            .into_iter()
            .find(|item| item.kind == "harness-prompt")
    };
    assert_eq!(
        prompt().unwrap().answers,
        vec!["allow".to_owned(), "deny".to_owned()]
    );
    assert_eq!(
        store
            .answer_native_prompt(seat, &asked.id, "allow", "person/other")
            .unwrap_err()
            .code,
        "forbidden"
    );
    assert_eq!(
        store
            .answer_native_prompt(seat, "an-older-prompt", "allow", "person/avery")
            .unwrap_err()
            .code,
        "stale-fence"
    );
    assert_eq!(
        store.native_prompt_state(seat, 1, 3).unwrap(),
        NativePromptState::Open
    );
    store
        .answer_native_prompt(seat, &asked.id, "deny", "person/avery")
        .unwrap();
    assert_eq!(
        store.native_prompt_state(seat, 1, 3).unwrap(),
        NativePromptState::Answered {
            answer: "deny".into()
        }
    );
    let answered = store.observed_harness_at(seat, store.index().unwrap()).unwrap().unwrap();
    assert_eq!(answered.blocked_on.as_deref(), Some("human"));
    assert_eq!(answered.ask.as_deref(), Some("permission"));
    // Recording the answer does not prove the native polling hook applied it.
    assert!(prompt().is_none(), "an answered prompt is no longer an alert");
    assert!(
        store
            .answer_native_prompt(seat, &asked.id, "allow", "person/avery")
            .is_err(),
        "a prompt is answered once"
    );
    // Claude acts on the answer and moves on: a hook still waiting sees it gone.
    append(
        "harness.observed",
        json!({"state":"working", "driver":"claude", "incarnation_id":"one",
            "blocked_on":null, "ask":null, "ownership_sequence":1, "transition_sequence":4}),
    );
    assert_eq!(
        store.native_prompt_state(seat, 1, 3).unwrap(),
        NativePromptState::Gone
    );
    // The next prompt is its own: the earlier prompt's answer never answers it.
    append(
        "harness.observed",
        json!({"state":"working", "driver":"claude", "incarnation_id":"one",
            "blocked_on":"human", "ask":"permission", "reason":"Write",
            "ownership_sequence":1, "transition_sequence":5}),
    );
    assert_eq!(
        store.native_prompt_state(seat, 1, 5).unwrap(),
        NativePromptState::Open
    );
    // Only the tool is stored; the call itself is read from the seat's conversation.
    assert!(prompt().unwrap().detail.starts_with("Claude asks to use Write;"));
    // A later observation of the same waiting prompt: an answer to it is still the hook's.
    let restated = append(
        "harness.observed",
        json!({"state":"working", "driver":"claude", "incarnation_id":"one",
            "blocked_on":"human", "ask":"permission", "reason":"Write",
            "provider_auth": true, "ownership_sequence":1, "transition_sequence":6}),
    );
    assert_eq!(prompt().unwrap().episode, restated.id);
    store
        .answer_native_prompt(seat, &restated.id, "allow", "person/avery")
        .unwrap();
    assert_eq!(
        store.native_prompt_state(seat, 1, 5).unwrap(),
        NativePromptState::Answered {
            answer: "allow".into()
        }
    );
    // A prompt that does not say what it would run can only be answered in the terminal.
    append(
        "harness.observed",
        json!({"state":"working", "driver":"claude", "incarnation_id":"one",
            "blocked_on":"human", "ask":"permission", "reason":"permissionRequest",
            "ownership_sequence":1, "transition_sequence":7}),
    );
    assert!(prompt().unwrap().answers.is_empty());
    // A question is answered in the terminal: it is an alert without answers.
    let question = append(
        "harness.observed",
        json!({"state":"working", "driver":"claude", "incarnation_id":"one",
            "blocked_on":"human", "ask":"question"}),
    );
    assert!(prompt().unwrap().answers.is_empty());
    assert_eq!(
        store
            .answer_native_prompt(seat, &question.id, "allow", "person/avery")
            .unwrap_err()
            .code,
        "stale-fence"
    );
}

/// A Claude prompt is answered from a client only while its permission hook still waits: one
/// that has waited longer is answered in the terminal, so no answer is recorded that nothing
/// would deliver. The hook still waiting on it is told it is gone, and exits.
#[test]
fn a_claude_prompt_older_than_its_hook_is_answered_in_the_terminal() {
    let store = Store::open_memory("node").unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let source = format!(
        "version 2\nagent \"worker\" {{ workspace {:?}; harness \"claude\" {{}} }}\n",
        workspace.path().display().to_string()
    );
    let intent = parse_intent(&source, "node").unwrap();
    let preview = store
        .mission(
            &intent,
            IntentInput {
                kdl: source,
                source_name: None,
            },
        )
        .unwrap();
    store
        .apply_as(&intent, &preview.subject_tokens, "late-seat", Some("person/avery"))
        .unwrap();
    let seat = "agent/node.worker";
    let append = |kind: &str, fields: Value| {
        store
            .append_claim(&ClaimInput {
                subject: seat.into(),
                kind: kind.into(),
                actor: Some(seat.into()),
                fields: serde_json::from_value(fields).unwrap(),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap()
    };
    let prompt = || {
        store
            .attention_items(Some("person/avery"))
            .unwrap()
            .into_iter()
            .find(|item| item.kind == "harness-prompt")
    };
    append("runtime.observed", json!({"status":"running", "incarnation_id":"one"}));
    let now = now_ms() as u64;
    let window = CLAUDE_PROMPT_ANSWER_WAIT_MS as u64;
    let asked = |observed_at: u64, transition: u64| {
        append(
            "harness.observed",
            json!({"state":"working", "driver":"claude", "incarnation_id":"one",
                "blocked_on":"human", "ask":"permission", "reason":"Read",
                "observed_at_ms": observed_at,
                "ownership_sequence":1, "transition_sequence":transition}),
        )
    };
    // Within the hook's wait, the prompt is answered here.
    asked(now - window + 60_000, 3);
    assert_eq!(prompt().unwrap().answers, vec!["allow".to_owned(), "deny".to_owned()]);
    // Expiring the hook only disables answering through st. The native dialog
    // stays open, and each restatement keeps its original opening time.
    append(
        "harness.observed",
        json!({"state":"working", "driver":"claude", "incarnation_id":"one",
            "blocked_on":null, "ask":null, "ownership_sequence":1, "transition_sequence":4}),
    );
    asked(now - window - 1_000, 5);
    let late = asked(now, 5);
    let shown = prompt().expect("a late prompt is still an alert");
    assert!(shown.answers.is_empty(), "{:?}", shown.answers);
    assert!(shown.detail.contains("waited longer than st can answer it"), "{}", shown.detail);
    assert_eq!(
        shown.requested_at_unix_ms,
        u128::from(now - window - 1_000),
        "it has waited since it opened, not since its latest restatement"
    );
    assert_eq!(
        store
            .answer_native_prompt(seat, &late.id, "allow", "person/avery")
            .unwrap_err()
            .code,
        "stale-fence"
    );
    assert_eq!(store.native_prompt_state(seat, 1, 5).unwrap(), NativePromptState::Open);
    let index = store.index().unwrap();
    let held = store.observed_harness_at(seat, index).unwrap().unwrap();
    assert_eq!(held.blocked_on.as_deref(), Some("human"),
        "hook expiry must retain the native human mail hold");
    assert_eq!(store.seat_observation_at(seat, Some(&held), index, now_ms()).unwrap(), "current");
    append(
        "harness.observed",
        json!({"state":"idle", "driver":"claude", "incarnation_id":"one",
            "blocked_on":null, "ask":null, "ownership_sequence":1, "transition_sequence":6}),
    );
    assert_eq!(store.native_prompt_state(seat, 1, 5).unwrap(), NativePromptState::Gone);
    assert!(store.observed_harness_at(seat, store.index().unwrap()).unwrap().unwrap().blocked_on.is_none(),
        "authoritative native closure releases the mail hold");
}

/// A driver writes each prompt as its own transition, so a later prompt for the same tool is
/// another prompt: an answer to it never reaches the earlier prompt's waiter. A restatement
/// of the same transition, or a credential edge, is still the same prompt.
#[test]
fn a_later_prompt_for_the_same_tool_is_another_prompt() {
    let store = Store::open_memory("node").unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let source = format!(
        "version 2\nagent \"worker\" {{ workspace {:?}; harness \"codex\" {{}} }}\n",
        workspace.path().display().to_string()
    );
    let intent = parse_intent(&source, "node").unwrap();
    let preview = store
        .mission(
            &intent,
            IntentInput {
                kdl: source,
                source_name: None,
            },
        )
        .unwrap();
    store
        .apply_as(&intent, &preview.subject_tokens, "two-prompts", Some("person/avery"))
        .unwrap();
    let seat = "agent/node.worker";
    let append = |fields: Value| {
        store
            .append_claim(&ClaimInput {
                subject: seat.into(),
                kind: "harness.observed".into(),
                actor: Some(seat.into()),
                fields: serde_json::from_value(fields).unwrap(),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap()
    };
    let waiting = |transition: u64, auth: Value| {
        json!({"state":"working", "driver":"codex", "incarnation_id":"one",
            "blocked_on":"human", "ask":"permission", "reason":"commandExecution",
            "provider_auth": auth, "ownership_sequence":1, "transition_sequence":transition})
    };
    store
        .append_claim(&ClaimInput {
            subject: seat.into(),
            kind: "runtime.observed".into(),
            actor: Some(seat.into()),
            fields: serde_json::from_value(json!({"status":"running", "incarnation_id":"one"}))
                .unwrap(),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    append(waiting(3, Value::Null));
    // Restated: the same prompt.
    append(waiting(3, Value::Null));
    assert_eq!(store.native_prompt_state(seat, 1, 3).unwrap(), NativePromptState::Open);
    // A credential edge moves the transition without a new prompt.
    let restated = append(waiting(4, json!(true)));
    store
        .answer_native_prompt(seat, &restated.id, "allow", "person/avery")
        .unwrap();
    assert_eq!(
        store.native_prompt_state(seat, 1, 3).unwrap(),
        NativePromptState::Answered {
            answer: "allow".into()
        }
    );
    // The next request is its own transition: its answer is never the earlier waiter's.
    let next = append(waiting(5, json!(true)));
    store
        .answer_native_prompt(seat, &next.id, "deny", "person/avery")
        .unwrap();
    assert_eq!(store.native_prompt_state(seat, 1, 3).unwrap(), NativePromptState::Gone);
    assert_eq!(
        store.native_prompt_state(seat, 1, 5).unwrap(),
        NativePromptState::Answered {
            answer: "deny".into()
        }
    );
}

/// Claude's question (AskUserQuestion) is answered from a client with the person's choices, as
/// positions (question to options chosen), or declined. Only positions are recorded; the hook
/// turns them into the call's own labels.
#[test]
fn a_claude_question_is_answered_with_its_choices() {
    let store = Store::open_memory("node").unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let source = format!(
        "version 2\nagent \"worker\" {{ workspace {:?}; harness \"claude\" {{}} }}\n",
        workspace.path().display().to_string()
    );
    let intent = parse_intent(&source, "node").unwrap();
    let preview = store
        .mission(
            &intent,
            IntentInput {
                kdl: source,
                source_name: None,
            },
        )
        .unwrap();
    store
        .apply_as(&intent, &preview.subject_tokens, "question-seat", Some("person/avery"))
        .unwrap();
    let seat = "agent/node.worker";
    let append = |kind: &str, fields: Value| {
        store
            .append_claim(&ClaimInput {
                subject: seat.into(),
                kind: kind.into(),
                actor: Some(seat.into()),
                fields: serde_json::from_value(fields).unwrap(),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap()
    };
    let prompt = || {
        store
            .attention_items(Some("person/avery"))
            .unwrap()
            .into_iter()
            .find(|item| item.kind == "harness-prompt")
    };
    append("runtime.observed", json!({"status":"running", "incarnation_id":"one"}));
    let asked = append(
        "harness.observed",
        json!({"state":"working", "driver":"claude", "incarnation_id":"one",
            "blocked_on":"human", "ask":"question", "reason":"AskUserQuestion",
            "ownership_sequence":1, "transition_sequence":3}),
    );
    let shown = prompt().unwrap();
    assert_eq!(shown.answers, vec!["answer".to_owned(), "deny".to_owned()]);
    assert!(shown.title.ends_with("is waiting for an answer"));
    assert!(shown.detail.starts_with("Claude asks you a question;"), "{}", shown.detail);
    // A question is never allowed, and its choices name each question with a choice.
    // Positions only: never the questions' or options' words, which are conversation content.
    for wrong in ["allow", "{}", r#"{"0":[]}"#, r#"["Blue"]"#, r#"{"Which color?":"Blue"}"#, r#"{"0":[1,1]}"#, r#"{"0":"1"}"#] {
        assert_eq!(
            store
                .answer_native_prompt(seat, &asked.id, wrong, "person/avery")
                .unwrap_err()
                .code,
            "invalid-answer",
            "{wrong}"
        );
    }
    let choices = r#"{"0":[1],"1":[0,2]}"#;
    store
        .answer_native_prompt(seat, &asked.id, choices, "person/avery")
        .unwrap();
    let NativePromptState::Answered { answer } = store.native_prompt_state(seat, 1, 3).unwrap()
    else {
        panic!("the question is answered");
    };
    assert_eq!(
        serde_json::from_str::<Value>(&answer).unwrap(),
        json!({"0": [1], "1": [0, 2]})
    );
    assert!(prompt().is_none());
    // The next question is declined.
    append(
        "harness.observed",
        json!({"state":"working", "driver":"claude", "incarnation_id":"one",
            "blocked_on":null, "ask":null, "ownership_sequence":1, "transition_sequence":4}),
    );
    let next = append(
        "harness.observed",
        json!({"state":"working", "driver":"claude", "incarnation_id":"one",
            "blocked_on":"human", "ask":"question", "reason":"AskUserQuestion",
            "ownership_sequence":1, "transition_sequence":5}),
    );
    store
        .answer_native_prompt(seat, &next.id, "deny", "person/avery")
        .unwrap();
    assert_eq!(
        store.native_prompt_state(seat, 1, 5).unwrap(),
        NativePromptState::Answered {
            answer: "deny".into()
        }
    );
    // A permission is never answered with choices.
    let permission = append(
        "harness.observed",
        json!({"state":"working", "driver":"claude", "incarnation_id":"one",
            "blocked_on":"human", "ask":"permission", "reason":"Bash",
            "ownership_sequence":1, "transition_sequence":6}),
    );
    assert_eq!(
        store
            .answer_native_prompt(seat, &permission.id, choices, "person/avery")
            .unwrap_err()
            .code,
        "invalid-answer"
    );
}

/// A subagent asks while its parent's Task call is still open: the only call the seat's
/// conversation shows unanswered is the parent's, so an allow offered beside it would let
/// through a call the person never saw. st offers only deny for a subagent's prompt.
#[test]
fn a_subagents_prompt_is_answered_only_with_deny() {
    let store = Store::open_memory("node").unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let source = format!(
        "version 2\nagent \"worker\" {{ workspace {:?}; harness \"claude\" {{}} }}\n",
        workspace.path().display().to_string()
    );
    let intent = parse_intent(&source, "node").unwrap();
    let preview = store
        .mission(
            &intent,
            IntentInput {
                kdl: source,
                source_name: None,
            },
        )
        .unwrap();
    store
        .apply_as(&intent, &preview.subject_tokens, "subagent-seat", Some("person/avery"))
        .unwrap();
    let seat = "agent/node.worker";
    let append = |kind: &str, fields: Value| {
        store
            .append_claim(&ClaimInput {
                subject: seat.into(),
                kind: kind.into(),
                actor: Some(seat.into()),
                fields: serde_json::from_value(fields).unwrap(),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap()
    };
    append("runtime.observed", json!({"status":"running", "incarnation_id":"one"}));
    // The seat's own Task call is open (its conversation shows it unanswered), and the
    // subagent it started asks to run Bash.
    let asked = append(
        "harness.observed",
        json!({"state":"working", "driver":"claude", "incarnation_id":"one",
            "blocked_on":"human", "ask":"permission", "reason":"subagent:Bash",
            "ownership_sequence":1, "transition_sequence":3}),
    );
    let shown = store
        .attention_items(Some("person/avery"))
        .unwrap()
        .into_iter()
        .find(|item| item.kind == "harness-prompt")
        .unwrap();
    assert_eq!(shown.answers, vec!["deny".to_owned()]);
    assert!(shown.detail.starts_with("A subagent of this seat asks to use Bash;"), "{}", shown.detail);
    assert_eq!(
        store
            .answer_native_prompt(seat, &asked.id, "allow", "person/avery")
            .unwrap_err()
            .code,
        "invalid-answer"
    );
    store
        .answer_native_prompt(seat, &asked.id, "deny", "person/avery")
        .unwrap();
    assert_eq!(
        store.native_prompt_state(seat, 1, 3).unwrap(),
        NativePromptState::Answered {
            answer: "deny".into()
        }
    );
}

/// A call Claude's auto mode classifier denied waits on the seat's person: an alert until they
/// answer it, and the answer (allow once, allow its kind, or dismiss) is what the seat's
/// `PreToolUse` hook reads; the seat is told, so it runs the call again or leaves it.
#[test]
fn a_classifier_denial_is_an_alert_whose_answer_the_seat_hears() {
    let store = Store::open_memory("node").unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let source = format!(
        "version 2\nagent \"worker\" {{ workspace {:?}; harness \"claude\" {{}} }}\n",
        workspace.path().display().to_string()
    );
    let intent = parse_intent(&source, "node").unwrap();
    let preview = store
        .mission(
            &intent,
            IntentInput {
                kdl: source,
                source_name: None,
            },
        )
        .unwrap();
    store
        .apply_as(&intent, &preview.subject_tokens, "denial-seat", Some("person/avery"))
        .unwrap();
    let seat = "agent/node.worker";
    let append = |kind: &str, fields: Value, key: Option<&str>| {
        store
            .append_claim(&ClaimInput {
                subject: seat.into(),
                kind: kind.into(),
                actor: Some(seat.into()),
                fields: serde_json::from_value(fields).unwrap(),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: key.map(str::to_owned),
            })
            .unwrap()
    };
    let denied = |call: &str| {
        append(
            "harness.diagnostic",
            json!({"severity":"warning", "status":"Bash", "code":"permission-denied",
                "reason":"Irreversible Local Destruction", "driver":"claude",
                "session_id":"session-1", "matched_line": json!({"call_id": call}).to_string()}),
            Some(&format!("claude-permission-denied:{seat}:{call}")),
        )
    };
    let alerts = || {
        store
            .attention_items(Some("person/avery"))
            .unwrap()
            .into_iter()
            .filter(|item| item.kind == "harness-prompt")
            .collect::<Vec<_>>()
    };
    append("runtime.observed", json!({"status":"running", "incarnation_id":"one"}), None);
    let first = denied("toolu_1");
    let shown = alerts();
    assert_eq!(shown.len(), 1, "{shown:?}");
    assert_eq!(shown[0].episode, first.id);
    assert_eq!(shown[0].answers, vec!["approve-once", "approve-kind", "dismiss"]);
    assert!(shown[0].title.ends_with("needs you to allow a Bash call"));
    assert!(shown[0].detail.contains("(Irreversible Local Destruction)"), "{}", shown[0].detail);
    assert_eq!(
        store.answer_native_prompt(seat, &first.id, "allow", "person/avery").unwrap_err().code,
        "invalid-answer"
    );
    assert_eq!(
        store.answer_native_prompt(seat, &first.id, "approve-once", "person/other").unwrap_err().code,
        "forbidden"
    );
    assert_eq!(store.prompt_answer(&first.id).unwrap().answer, None);
    store.answer_native_prompt(seat, &first.id, "approve-once", "person/avery").unwrap();
    assert_eq!(store.prompt_answer(&first.id).unwrap().answer.as_deref(), Some("approve-once"));
    assert!(alerts().is_empty(), "an answered denial is no longer an alert");
    // The seat hears it, from st, and runs the call again.
    let told = store
        .claims_for(&format!(
            "message/{}",
            &hex::encode(sha2::Sha256::digest(format!("st3-denial-answer:{}", first.id).as_bytes()))[..16]
        ), Some("message.sent"))
        .unwrap();
    assert_eq!(told.len(), 1);
    assert_eq!(told[0].body["fields"]["to"], seat);
    assert!(told[0].body["fields"]["content"].as_str().unwrap().contains("Run that same call again"));
    assert_eq!(
        store.answer_native_prompt(seat, &first.id, "dismiss", "person/avery").unwrap_err().code,
        "stale-fence",
        "an answer is final"
    );
    // A denial from before the seat restarted waits on nobody.
    let old = denied("toolu_2");
    assert_eq!(alerts().len(), 1);
    append("runtime.observed", json!({"status":"exited", "incarnation_id":"one"}), None);
    append("runtime.observed", json!({"status":"running", "incarnation_id":"two"}), None);
    assert!(alerts().is_empty());
    assert_eq!(
        store.answer_native_prompt(seat, &old.id, "approve-kind", "person/avery").unwrap_err().code,
        "stale-fence"
    );
}

/// A prompt names the exact call it asks about, as its hook published it for that very
/// observation; a later prompt's call is never an earlier one's, and an unnamed call is none.
#[test]
fn a_prompt_names_the_exact_call_its_hook_published() {
    let store = Store::open_memory("node").unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let source = format!(
        "version 2\nagent \"worker\" {{ workspace {:?}; harness \"claude\" {{}} }}\n",
        workspace.path().display().to_string()
    );
    let intent = parse_intent(&source, "node").unwrap();
    let preview = store
        .mission(
            &intent,
            IntentInput {
                kdl: source,
                source_name: None,
            },
        )
        .unwrap();
    store
        .apply_as(&intent, &preview.subject_tokens, "call-seat", Some("person/avery"))
        .unwrap();
    let seat = "agent/node.worker";
    let append = |kind: &str, fields: Value| {
        store
            .append_claim(&ClaimInput {
                subject: seat.into(),
                kind: kind.into(),
                actor: Some(seat.into()),
                fields: serde_json::from_value(fields).unwrap(),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap()
    };
    let call_of_prompt = || {
        store
            .attention_items(Some("person/avery"))
            .unwrap()
            .into_iter()
            .find(|item| item.kind == "harness-prompt")
            .unwrap()
            .prompt_call
    };
    let asked = |transition: u64| {
        append(
            "harness.observed",
            json!({"state":"working", "driver":"claude", "incarnation_id":"one",
                "blocked_on":"human", "ask":"permission", "reason":"Bash",
                "ownership_sequence":1, "transition_sequence":transition}),
        )
    };
    let published = |transition: u64, call: &str| {
        append(
            "harness.diagnostic",
            json!({"severity":"warning", "status":"shown", "code":"native-prompt-call",
                "driver":"claude", "ownership_sequence":1,
                "matched_line": json!({"call_id": call, "transition": transition}).to_string()}),
        )
    };
    append("runtime.observed", json!({"status":"running", "incarnation_id":"one"}));
    asked(3);
    assert_eq!(call_of_prompt(), None, "not named yet: allowed only in the terminal");
    published(3, "toolu_7");
    assert_eq!(call_of_prompt().as_deref(), Some("toolu_7"));
    // The next prompt is its own observation: the earlier call is not its call.
    append(
        "harness.observed",
        json!({"state":"working", "driver":"claude", "incarnation_id":"one",
            "blocked_on":null, "ask":null, "ownership_sequence":1, "transition_sequence":4}),
    );
    asked(5);
    assert_eq!(call_of_prompt(), None);
    published(5, "toolu_9");
    assert_eq!(call_of_prompt().as_deref(), Some("toolu_9"));
}

/// A denied call waits on its person only until the seat moves on: once the seat begins another
/// turn or its session ends, the alert goes and an answer is refused. The turn the denial ended
/// is still observed working for a moment, and that is no new turn; nor is the seat at rest,
/// however often it is observed. The claims stay: only the alert they derived goes.
#[test]
fn a_classifier_denial_goes_once_the_seat_moves_on() {
    let store = Store::open_memory("node").unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let source = format!(
        "version 2\nagent \"worker\" {{ workspace {:?}; harness \"claude\" {{}} }}\n",
        workspace.path().display().to_string()
    );
    let intent = parse_intent(&source, "node").unwrap();
    let preview = store
        .mission(
            &intent,
            IntentInput {
                kdl: source,
                source_name: None,
            },
        )
        .unwrap();
    store
        .apply_as(&intent, &preview.subject_tokens, "denial-seat", Some("person/avery"))
        .unwrap();
    let seat = "agent/node.worker";
    let append = |kind: &str, fields: Value, key: Option<&str>| {
        store
            .append_claim(&ClaimInput {
                subject: seat.into(),
                kind: kind.into(),
                actor: Some(seat.into()),
                fields: serde_json::from_value(fields).unwrap(),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: key.map(str::to_owned),
            })
            .unwrap()
    };
    let denied = |call: &str| {
        append(
            "harness.diagnostic",
            json!({"severity":"warning", "status":"Bash", "code":"permission-denied",
                "reason":"Production Reads", "driver":"claude",
                "session_id":"session-1", "matched_line": json!({"call_id": call}).to_string()}),
            Some(&format!("claude-permission-denied:{seat}:{call}")),
        )
    };
    // The driver names when it saw each state; the store keeps that as the state's start when
    // the state changes, and carries the start over a refresh of the same state.
    let observed = |state: &str, at: Option<u128>| {
        let mut fields = json!({"state": state, "driver": "claude", "blocked_on": "none", "ask": "none"});
        if let Some(at) = at {
            fields["observed_at_ms"] = json!(at as u64);
        }
        append("harness.observed", fields, None)
    };
    let alerts = || {
        store
            .attention_items(Some("person/avery"))
            .unwrap()
            .into_iter()
            .filter(|item| item.kind == "harness-prompt")
            .map(|item| item.episode)
            .collect::<Vec<_>>()
    };
    append("runtime.observed", json!({"status":"running", "incarnation_id":"one"}), None);
    let first = denied("toolu_1");
    // The turn that ended in the denial, then the seat at rest, both seen before the `Stop` hook
    // raised it: still waiting on the person.
    let ending = observed("working", Some(first.accepted_at_unix_ms - 1_000));
    assert_eq!(ending.body["fields"]["observed_since_ms"], (first.accepted_at_unix_ms - 1_000) as u64);
    observed("idle", Some(first.accepted_at_unix_ms - 500));
    assert_eq!(alerts(), vec![first.id.clone()]);
    // However long the seat rests, refreshed as it is, the call still waits.
    for refresh in 1..=40 {
        observed("idle", Some(first.accepted_at_unix_ms + refresh * 300_000));
    }
    assert_eq!(alerts(), vec![first.id.clone()], "a resting seat's call still waits");
    // Another turn begins: the call waits on nobody.
    std::thread::sleep(std::time::Duration::from_millis(5));
    observed("working", None);
    assert!(alerts().is_empty(), "the seat moved on");
    assert_eq!(
        store.answer_native_prompt(seat, &first.id, "approve-once", "person/avery").unwrap_err().code,
        "stale-fence"
    );
    // A session that ends leaves nothing waiting either.
    let second = denied("toolu_2");
    assert_eq!(alerts(), vec![second.id.clone()]);
    observed("ended", None);
    assert!(alerts().is_empty(), "the session ended");
}

/// A Codex approval prompt is answered the same way. The answer names the Codex driver, whose
/// control connection reads it and answers the app-server; a Codex question stays terminal-only.
#[test]
fn a_codex_permission_prompt_is_answered_and_records_its_driver() {
    let store = Store::open_memory("node").unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let source = format!(
        "version 2\nagent \"worker\" {{ workspace {:?}; harness \"codex\" {{}} }}\n",
        workspace.path().display().to_string()
    );
    let intent = parse_intent(&source, "node").unwrap();
    let preview = store
        .mission(
            &intent,
            IntentInput {
                kdl: source,
                source_name: None,
            },
        )
        .unwrap();
    store
        .apply_as(&intent, &preview.subject_tokens, "answer-seat", Some("person/avery"))
        .unwrap();
    let seat = "agent/node.worker";
    let append = |kind: &str, fields: Value| {
        store
            .append_claim(&ClaimInput {
                subject: seat.into(),
                kind: kind.into(),
                actor: Some(seat.into()),
                fields: serde_json::from_value(fields).unwrap(),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap()
    };
    let prompt = || {
        store
            .attention_items(Some("person/avery"))
            .unwrap()
            .into_iter()
            .find(|item| item.kind == "harness-prompt")
    };
    append("runtime.observed", json!({"status":"running", "incarnation_id":"one"}));
    // A wait the driver knows only from the thread's flag has no request st could answer.
    let flagged = append(
        "harness.observed",
        json!({"state":"working", "driver":"codex", "incarnation_id":"one",
            "blocked_on":"human", "ask":"permission", "reason":"waitingOnApproval",
            "ownership_sequence":1, "transition_sequence":2}),
    );
    assert!(prompt().unwrap().answers.is_empty());
    assert!(prompt().unwrap().detail.contains("did not see requested"));
    assert_eq!(
        store
            .answer_native_prompt(seat, &flagged.id, "allow", "person/avery")
            .unwrap_err()
            .code,
        "stale-fence"
    );
    // The driver waits on the observation its approval wrote, named by the record's sequences,
    // and the observation names what the request asks to do.
    let asked = append(
        "harness.observed",
        json!({"state":"working", "driver":"codex", "incarnation_id":"one",
            "blocked_on":"human", "ask":"permission", "reason":"commandExecution",
            "ownership_sequence":1, "transition_sequence":3}),
    );
    assert_eq!(
        prompt().unwrap().answers,
        vec!["allow".to_owned(), "deny".to_owned()]
    );
    assert!(prompt().unwrap().detail.starts_with("Codex asks to run a command;"));
    assert_eq!(
        store.native_prompt_state(seat, 1, 3).unwrap(),
        NativePromptState::Open
    );
    let answer = store
        .answer_native_prompt(seat, &asked.id, "allow", "person/avery")
        .unwrap();
    assert_eq!(answer.body["fields"]["driver"], "codex");
    assert_eq!(
        store.native_prompt_state(seat, 1, 3).unwrap(),
        NativePromptState::Answered {
            answer: "allow".into()
        }
    );
    assert!(prompt().is_none(), "an answered prompt is no longer an alert");
    // The next approval is its own: the earlier answer never answers it.
    append(
        "harness.observed",
        json!({"state":"working", "driver":"codex", "incarnation_id":"one",
            "blocked_on":null, "ask":null, "ownership_sequence":1, "transition_sequence":4}),
    );
    append(
        "harness.observed",
        json!({"state":"working", "driver":"codex", "incarnation_id":"one",
            "blocked_on":"human", "ask":"permission", "reason":"fileChange",
            "ownership_sequence":1, "transition_sequence":5}),
    );
    assert!(prompt().unwrap().detail.starts_with("Codex asks to change files;"));
    assert_eq!(
        store.native_prompt_state(seat, 1, 3).unwrap(),
        NativePromptState::Gone
    );
    assert_eq!(
        store.native_prompt_state(seat, 1, 5).unwrap(),
        NativePromptState::Open
    );
    let question = append(
        "harness.observed",
        json!({"state":"working", "driver":"codex", "incarnation_id":"one",
            "blocked_on":"human", "ask":"question", "reason":"waitingOnUserInput",
            "ownership_sequence":1, "transition_sequence":6}),
    );
    assert!(prompt().unwrap().answers.is_empty());
    assert_eq!(
        store
            .answer_native_prompt(seat, &question.id, "allow", "person/avery")
            .unwrap_err()
            .code,
        "stale-fence"
    );
}

#[test]
fn harness_projection_is_bound_to_the_current_runtime_epoch() {
    let store = Store::open_memory("node").unwrap();
    let subject = "agent/node.worker";
    let runtime = |incarnation: &str, key: &str| {
        store
            .append_claim(&ClaimInput {
                subject: subject.into(),
                kind: "runtime.observed".into(),
                actor: None,
                fields: BTreeMap::from([
                    ("status".into(), Value::String("running".into())),
                    ("runtime_id".into(), Value::String("node.worker".into())),
                    ("terminal".into(), Value::Bool(true)),
                    ("incarnation_id".into(), Value::String(incarnation.into())),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some(key.into()),
            })
            .unwrap();
    };
    let harness = |incarnation: Option<&str>, key: &str| {
        let mut fields = BTreeMap::from([
            ("state".into(), Value::String("ready".into())),
            ("driver".into(), Value::String("codex".into())),
            ("transport".into(), Value::String("app-server".into())),
        ]);
        if let Some(incarnation) = incarnation {
            fields.insert("incarnation_id".into(), Value::String(incarnation.into()));
        }
        store
            .append_claim(&ClaimInput {
                subject: subject.into(),
                kind: "harness.observed".into(),
                actor: Some(subject.into()),
                fields,
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some(key.into()),
            })
            .unwrap();
    };

    runtime("old", "runtime-old");
    harness(Some("old"), "harness-old");
    assert_eq!(
        store
            .current_harness(subject)
            .unwrap()
            .unwrap()
            .incarnation_id,
        "old"
    );

    runtime("new", "runtime-new");
    assert!(store.current_harness(subject).unwrap().is_none());
    assert!(
        store
            .latest_actual_value(subject)
            .unwrap()
            .unwrap()
            .get("state")
            .is_none()
    );

    harness(Some("wrong"), "harness-wrong");
    assert!(store.current_harness(subject).unwrap().is_none());
    harness(None, "harness-legacy-current-epoch");
    assert_eq!(
        store
            .current_harness(subject)
            .unwrap()
            .unwrap()
            .incarnation_id,
        "new"
    );
    harness(Some("new"), "harness-new");
    // The harness-only status a driver renews its leases from answers what the full one does,
    // without the seat's claim ids; the fence's newest-claim seek names the last claim.
    let only = store.status_harness_only(subject).unwrap();
    assert_eq!(only.subjects.len(), 1);
    assert!(only.subjects[0].claims.is_empty());
    assert_eq!(
        only.subjects[0].harness.as_ref().map(|harness| harness.incarnation_id.clone()),
        store.current_harness(subject).unwrap().map(|harness| harness.incarnation_id)
    );
    assert_eq!(
        store.latest_claim_id(subject).unwrap(),
        store.claims_for(subject, None).unwrap().last().map(|claim| claim.id.clone())
    );
    store
        .append_claim(&ClaimInput {
            subject: subject.into(),
            kind: "harness.observed".into(),
            actor: Some(subject.into()),
            fields: BTreeMap::from([
                ("state".into(), Value::String("working".into())),
                ("driver".into(), Value::String("codex".into())),
                ("incarnation_id".into(), Value::String("new".into())),
                ("reason".into(), Value::Null),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some("harness-new-activity".into()),
        })
        .unwrap();
    let current = store.current_harness(subject).unwrap().unwrap();
    assert_eq!(current.incarnation_id, "new");
    assert_eq!(current.state, "working");
    assert!(current.is_ready());
    assert_eq!(current.driver.as_deref(), Some("codex"));
    assert_eq!(current.transport.as_deref(), Some("app-server"));
    assert_eq!(current.reason, None);

    let mut complete = BTreeMap::from([
        ("state".into(), Value::String("ready".into())),
        ("driver".into(), Value::String("codex".into())),
        ("transport".into(), Value::String("app-server".into())),
        ("incarnation_id".into(), Value::String("new".into())),
    ]);
    for field in ["reason", "blocked_on", "ask", "input_buffer", "exit"] {
        complete.insert(field.into(), Value::Null);
    }
    store
        .append_claim(&ClaimInput {
            subject: subject.into(),
            kind: "harness.observed".into(),
            actor: Some(subject.into()),
            fields: complete,
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some("harness-new-complete-snapshot".into()),
        })
        .unwrap();
    let complete = store.current_harness(subject).unwrap().unwrap();
    assert_eq!(complete.state, "ready");
    assert_eq!(complete.transport.as_deref(), Some("app-server"));
    assert_eq!(complete.reason, None);

    store
        .append_claim(&ClaimInput {
            subject: subject.into(),
            kind: "runtime.readiness-deadline-reached".into(),
            actor: None,
            fields: BTreeMap::from([
                ("runtime_id".into(), Value::String("node.worker".into())),
                ("driver".into(), Value::String("codex".into())),
                ("incarnation_id".into(), Value::String("new".into())),
                (
                    "deadline_unix_ms".into(),
                    Value::String("1700000000000".into()),
                ),
                (
                    "reason".into(),
                    Value::String("the harness did not become ready".into()),
                ),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some("readiness-deadline".into()),
        })
        .unwrap();
    let actual = store.latest_actual_value(subject).unwrap().unwrap();
    assert!(actual.get("deadline_unix_ms").is_none());
    assert!(actual.get("reason").is_none());
}

#[test]
fn work_orphaning_requires_terminal_or_replacement_runtime_evidence() {
    let store = Store::open_memory("node").unwrap();
    let subject = "agent/node.worker";
    let work: StepRunView = serde_json::from_value(serde_json::json!({
        "subject": "step-run/run/work",
        "run": "mission-run/run",
        "generation": "run-generation/run",
        "step": "work",
        "definition_hash": "definition",
        "status": "blocked",
        "attempt": 1,
        "assigned_to": subject,
        "agentless": false,
        "title": null,
        "worker_reported": false,
        "claimant": subject,
        "claim_incarnation": "worker-one",
        "claim_expires_at_unix_ms": 10,
        "execution_elapsed_ms": 0,
        "readiness_epoch": 1,
        "blocked_reason": "waiting",
        "not_before_unix_ms": null,
        "created_at_unix_ms": 1,
        "updated_at_unix_ms": 1
    }))
    .unwrap();
    let observe = |status: &str, incarnation: Option<&str>, key: &str| {
        let mut fields = BTreeMap::from([("status".into(), Value::String(status.to_owned()))]);
        if let Some(incarnation) = incarnation {
            fields.insert("incarnation_id".into(), Value::String(incarnation.into()));
        }
        store
            .append_claim(&ClaimInput {
                subject: subject.into(),
                kind: "runtime.observed".into(),
                actor: None,
                fields,
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some(key.into()),
            })
            .unwrap();
    };

    assert!(!store.work_claim_is_orphaned(&work).unwrap());
    observe("starting", None, "orphan-starting");
    assert!(!store.work_claim_is_orphaned(&work).unwrap());
    observe("running", Some("worker-one"), "orphan-current");
    assert!(!store.work_claim_is_orphaned(&work).unwrap());
    observe("starting", None, "orphan-inconclusive-after-current");
    assert!(!store.work_claim_is_orphaned(&work).unwrap());
    observe("running", Some("worker-two"), "orphan-replacement");
    assert!(store.work_claim_is_orphaned(&work).unwrap());
    observe("stopped", None, "orphan-stopped");
    assert!(store.work_claim_is_orphaned(&work).unwrap());
}

#[test]
fn current_incarnation_work_activity_recovers_a_stale_harness_projection() {
    let store = Store::open_memory("node").unwrap();
    let subject = "agent/node.worker";
    store
        .append_claim(&ClaimInput {
            subject: subject.into(),
            kind: "runtime.observed".into(),
            actor: None,
            fields: BTreeMap::from([
                ("status".into(), Value::String("running".into())),
                ("runtime_id".into(), Value::String("node.worker".into())),
                ("terminal".into(), Value::Bool(true)),
                ("incarnation_id".into(), Value::String("current".into())),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some("activity-runtime".into()),
        })
        .unwrap();
    store
        .append_claim(&ClaimInput {
            subject: subject.into(),
            kind: "harness.observed".into(),
            actor: Some(subject.into()),
            fields: BTreeMap::from([
                ("state".into(), Value::String("indeterminate".into())),
                ("driver".into(), Value::String("codex".into())),
                ("incarnation_id".into(), Value::String("current".into())),
                ("reason".into(), Value::String("stale".into())),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some("activity-stale".into()),
        })
        .unwrap();
    assert_eq!(
        store
            .current_harness(subject)
            .unwrap()
            .unwrap()
            .reason
            .as_deref(),
        Some("stale")
    );

    {
        let mut connection = store.connection.lock().unwrap();
        let transaction = connection.transaction().unwrap();
        append_claim_tx(
            &transaction,
            "node",
            "step-run/activity/work",
            "work.claimed",
            Some(subject),
            &json!({"fields": {
                "status": "claimed",
                "attempt": 1,
                "readiness_epoch": 1,
                "claimant": subject,
                "claim_incarnation": "current",
                "claim_expires_at_unix_ms": now_ms().saturating_add(60_000),
                "worker_reported": false,
                "summary": null,
                "reason": null
            }}),
            &[],
            None,
        )
        .unwrap();
        transaction.commit().unwrap();
    }

    let recovered = store.current_harness(subject).unwrap().unwrap();
    assert_eq!(recovered.state, "working");
    assert!(recovered.is_ready());
    assert_eq!(recovered.reason, None);
    assert_eq!(recovered.driver.as_deref(), Some("codex"));

    // A lease renewal says only that the process is alive enough to extend its lease.
    // It must not erase a newer observation that the harness is stuck in a command.
    store
        .append_claim(&ClaimInput {
            subject: subject.into(),
            kind: "harness.observed".into(),
            actor: Some(subject.into()),
            fields: BTreeMap::from([
                ("state".into(), Value::String("indeterminate".into())),
                ("driver".into(), Value::String("codex".into())),
                ("incarnation_id".into(), Value::String("current".into())),
                ("reason".into(), Value::String("stale-command".into())),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some("activity-command-stale".into()),
        })
        .unwrap();
    {
        let mut connection = store.connection.lock().unwrap();
        let transaction = connection.transaction().unwrap();
        append_claim_tx(
            &transaction,
            "node",
            "step-run/activity/work",
            "work.renewed",
            Some(subject),
            &json!({"fields": {
                "status": "claimed",
                "attempt": 1,
                "readiness_epoch": 1,
                "claimant": subject,
                "claim_incarnation": "current",
                "claim_expires_at_unix_ms": now_ms().saturating_add(60_000),
                "worker_reported": false,
                "summary": null,
                "reason": null
            }}),
            &[],
            None,
        )
        .unwrap();
        transaction.commit().unwrap();
    }
    let renewed = store.current_harness(subject).unwrap().unwrap();
    assert_eq!(renewed.state, "indeterminate");
    assert_eq!(renewed.reason.as_deref(), Some("stale-command"));

    {
        let mut connection = store.connection.lock().unwrap();
        let transaction = connection.transaction().unwrap();
        append_claim_tx(
            &transaction,
            "node",
            "step-run/activity/work",
            "work.progress",
            Some(subject),
            &json!({"fields": {
                "status": "working",
                "attempt": 1,
                "readiness_epoch": 1,
                "claimant": subject,
                "claim_incarnation": "current",
                "claim_expires_at_unix_ms": now_ms().saturating_add(60_000),
                "worker_reported": false,
                "summary": "command finished",
                "reason": null
            }}),
            &[],
            None,
        )
        .unwrap();
        transaction.commit().unwrap();
    }
    assert_eq!(
        store.current_harness(subject).unwrap().unwrap().state,
        "working"
    );
}

#[test]
fn usage_aggregation_separates_occupancy_and_deduplicates_cumulative_incarnations() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(&directory.path().join("usage.sqlite"), "usage-node").unwrap();
    let subject = "agent/usage-owner";
    let append = |semantics: &str,
                  incarnation: &str,
                  total: Option<u64>,
                  context_used: Option<u64>,
                  key: &str| {
        let mut fields = BTreeMap::from([
            ("semantics".into(), Value::String(semantics.into())),
            ("driver".into(), Value::String("codex".into())),
            ("incarnation_id".into(), Value::String(incarnation.into())),
        ]);
        if let Some(total) = total {
            fields.insert("total_tokens".into(), Value::from(total));
        }
        if let Some(used) = context_used {
            fields.insert("context_used_tokens".into(), Value::from(used));
        }
        store
            .append_claim(&ClaimInput {
                subject: subject.into(),
                kind: "harness.usage".into(),
                actor: Some(subject.into()),
                fields,
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some(key.into()),
            })
            .unwrap();
    };
    append("session_cumulative", "one", Some(100), None, "one-100");
    append("response", "one", Some(10), None, "one-response");
    append("session_cumulative", "one", Some(150), None, "one-150");
    append("response", "two", Some(20), None, "two-response-1");
    append("response", "two", Some(30), None, "two-response-2");
    append(
        "context_occupancy",
        "two",
        None,
        Some(999),
        "occupancy-arrived-last",
    );

    // The default path must use a SQLite-representable bound rather than u64::MAX.
    let usage = store
        .usage_summary_at(subject, None, None)
        .unwrap()
        .unwrap();
    let batch = store
        .usage_summaries_at(
            &[subject.into(), "agent/missing".into(), subject.into()],
            None,
        )
        .unwrap();
    assert_eq!(batch.get(subject), Some(&usage));
    assert!(!batch.contains_key("agent/missing"));
    assert_eq!(usage.total_tokens, 200);
    assert_eq!(usage.incarnation_count, 2);
    assert_eq!(usage.context.unwrap().used_tokens, Some(999));
    assert_eq!(
        store
            .usage_summary_at(subject, Some("one"), None)
            .unwrap()
            .unwrap()
            .total_tokens,
        150
    );
}

#[test]
fn batched_usage_preserves_canonical_ties_and_snapshot_bound() {
    let store = Store::open_memory("usage-node").unwrap();
    let subjects = ["agent/usage-a", "agent/usage-b"];
    let mut middle = 0;
    for i in 0..602 {
        let subject = subjects[i % 2];
        store
            .append_claim(&ClaimInput {
                subject: subject.into(),
                kind: "harness.usage".into(),
                actor: Some(subject.into()),
                fields: BTreeMap::from([
                    ("driver".into(), json!("codex")),
                    ("incarnation_id".into(), json!("same-incarnation")),
                    (
                        "semantics".into(),
                        json!(if i % 4 < 2 {
                            "session_cumulative"
                        } else {
                            "context_occupancy"
                        }),
                    ),
                    ("total_tokens".into(), json!(100)),
                    ("cost".into(), json!(i as f64)),
                    ("context_used_tokens".into(), json!(i)),
                    ("model".into(), json!("fixture-model-".repeat(320))),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        if i == 299 {
            middle = store.index().unwrap();
        }
    }
    // Reverse timestamps and batch order: tied canonical winners differ from index row order.
    {
        let mut connection = store.connection.write();
        let tx = connection.transaction().unwrap();
        tx.execute(
            "UPDATE claims SET accepted_at_unix_ms=CAST(1000000-(store_index/8) AS TEXT)
                        WHERE kind='harness.usage'",
            [],
        )
        .unwrap();
        tx.execute(
            "UPDATE batches SET replica_sequence=10000-(SELECT MIN(store_index)
                        FROM claims WHERE claims.batch_id=batches.id)",
            [],
        )
        .unwrap();
        tx.commit().unwrap();
    }
    let selected = [
        subjects[1].into(),
        subjects[0].into(),
        subjects[1].into(),
        "agent/missing".into(),
    ];
    for at in [None, Some(middle)] {
        let batch = store.usage_summaries_at(&selected, at).unwrap();
        assert_eq!(batch.len(), 2);
        assert_eq!(batch[subjects[0]].cost, Some(0.0));
        assert_eq!(batch[subjects[1]].cost, Some(1.0));
        assert_eq!(
            batch[subjects[0]].context.as_ref().unwrap().used_tokens,
            Some(2)
        );
        for subject in subjects {
            assert_eq!(
                batch.get(subject),
                store.usage_summary_at(subject, None, at).unwrap().as_ref()
            );
        }
    }
}

fn claim_id_order_fixture(connection: &Connection) {
    connection
        .execute_batch(
            "CREATE TABLE batches(id TEXT PRIMARY KEY, origin TEXT, replica_sequence INTEGER);
             CREATE TABLE claims(id TEXT PRIMARY KEY, subject TEXT, kind TEXT, batch_id TEXT,
                                 store_index INTEGER, accepted_at_unix_ms TEXT);
             CREATE INDEX claims_subject_accepted_index ON claims
                 (subject, length(accepted_at_unix_ms), accepted_at_unix_ms);
             CREATE TABLE replica_records(claim_id TEXT, position INTEGER);
             INSERT INTO batches VALUES ('x','writer-a',4),('y','writer-a',3),('z','writer-b',1);
             INSERT INTO claims VALUES
               ('early','agent/target','runtime.observed','x',1,'9'),
               ('legacy','agent/target','harness.usage','x',2,'10'),
               ('record-a','agent/target','work.renewed','x',3,'10'),
               ('other-subject','agent/other','daemon.diagnostic','x',4,'10'),
               ('record-b','agent/target','intent.desired','x',5,'10'),
               ('cross-batch','agent/target','harness.observed','y',6,'10'),
               ('other-writer','agent/target','runtime.observed','z',7,'10'),
               ('u128-time','agent/target','harness.usage','z',8,
                '340282366920938463463374607431768211455');
             INSERT INTO replica_records VALUES ('record-a',7),('record-a',3),('record-b',0);",
        )
        .unwrap();
}

fn full_canonical_claim_id_oracle(
    connection: &Connection,
    subject: &str,
    at: Option<u64>,
) -> Vec<String> {
    connection
        .prepare(&canonical_sql(
            "SELECT id FROM claims WHERE subject=?1 AND store_index<=?2
             ORDER BY CANONICAL_ASC(claims)",
        ))
        .unwrap()
        .query_map(params![subject, at.unwrap_or(i64::MAX as u64)], |row| {
            row.get::<_, String>(0)
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
}

#[test]
fn claim_ids_stream_preserves_full_order_historical_bounds_and_transaction_ownership() {
    let mut connection = Connection::open_in_memory().unwrap();
    claim_id_order_fixture(&connection);
    assert_eq!(
        full_canonical_claim_id_oracle(&connection, "agent/target", None),
        [
            "early",
            "cross-batch",
            "record-b",
            "legacy",
            "record-a",
            "other-writer",
            "u128-time"
        ]
    );
    for at in [None, Some(2), Some(5), Some(6)] {
        let expected = full_canonical_claim_id_oracle(&connection, "agent/target", at);
        let actual = claim_ids_at(&connection, "agent/target", at).unwrap();
        assert_eq!(actual, expected);
        assert_eq!(
            actual.last(),
            expected.last(),
            "the planning revision stays exact"
        );
        assert!(connection.is_autocommit());
    }
    assert!(
        claim_ids_at(&connection, "agent/missing", None)
            .unwrap()
            .is_empty()
    );
    assert!(connection.is_autocommit());
    let tx = connection.transaction().unwrap();
    tx.execute(
        "UPDATE replica_records SET position=0 WHERE claim_id='record-a'",
        [],
    )
    .unwrap();
    assert_eq!(
        claim_ids_at(&tx, "agent/target", None).unwrap(),
        full_canonical_claim_id_oracle(&tx, "agent/target", None)
    );
    assert!(
        !tx.is_autocommit(),
        "the caller retains its in-flight writer transaction"
    );
    tx.rollback().unwrap();
    assert!(connection.is_autocommit());
    connection
        .execute_batch("PRAGMA query_only=ON; BEGIN")
        .unwrap();
    assert_eq!(
        claim_ids_at(&connection, "agent/target", None).unwrap(),
        full_canonical_claim_id_oracle(&connection, "agent/target", None)
    );
    assert!(
        !connection.is_autocommit(),
        "the caller retains its read transaction"
    );
    connection
        .execute_batch("ROLLBACK; PRAGMA query_only=OFF")
        .unwrap();
    connection
        .execute_batch("DROP TABLE replica_records")
        .unwrap();
    assert!(claim_ids_at(&connection, "agent/target", None).is_err());
    assert!(
        connection.is_autocommit(),
        "a failed owned read must roll back"
    );
    let tx = connection.transaction().unwrap();
    assert!(claim_ids_at(&tx, "agent/target", None).is_err());
    assert!(
        !tx.is_autocommit(),
        "a failed nested read must not roll back its caller"
    );
    tx.rollback().unwrap();
    connection
        .execute_batch("PRAGMA query_only=ON; BEGIN")
        .unwrap();
    assert!(claim_ids_at(&connection, "agent/target", None).is_err());
    assert!(
        !connection.is_autocommit(),
        "a failed read must not close its caller"
    );
    connection
        .execute_batch("ROLLBACK; PRAGMA query_only=OFF")
        .unwrap();
}

#[test]
fn claim_ids_stream_pins_tie_lookups_across_a_wal_writer_and_unwind() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("claims.sqlite3");
    let connection = Connection::open(&path).unwrap();
    connection.execute_batch("PRAGMA journal_mode=WAL").unwrap();
    claim_id_order_fixture(&connection);
    let old = full_canonical_claim_id_oracle(&connection, "agent/target", None);
    let actual = claim_ids_at_after_first_row(&connection, "agent/target", None, || {
        let writer = Connection::open(&path).unwrap();
        writer
            .execute_batch(
                "BEGIN IMMEDIATE;
                 UPDATE replica_records SET position=0 WHERE claim_id='record-a';
                 UPDATE replica_records SET position=99 WHERE claim_id='record-b';
                 COMMIT;",
            )
            .unwrap();
    })
    .unwrap();
    assert_eq!(
        actual, old,
        "the first stream row and later tie keys share one cut"
    );
    assert!(connection.is_autocommit());
    let new = full_canonical_claim_id_oracle(&connection, "agent/target", None);
    assert_ne!(
        new, old,
        "the concurrent commit really changed the canonical order"
    );
    assert_eq!(
        claim_ids_at(&connection, "agent/target", None).unwrap(),
        new
    );
    let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        claim_ids_at_after_first_row(&connection, "agent/target", None, || {
            panic!("injected after stream pin")
        })
    }));
    assert!(unwind.is_err());
    assert!(
        connection.is_autocommit(),
        "unwind must release the locally owned read"
    );
    assert_eq!(
        claim_ids_at(&connection, "agent/target", None).unwrap(),
        new
    );
}
#[test]
fn claim_ids_stream_keeps_real_store_status_claims_at_current_and_historical_cuts() {
    let store = Store::open_memory("claim-id-node").unwrap();
    let subject = "resource/claim-id-order";
    let mut historical = 0;
    for i in 0..4 {
        store
            .append_claim(&ClaimInput {
                subject: subject.into(),
                kind: "resource.observed".into(),
                actor: Some("person/avery".into()),
                fields: BTreeMap::from([
                    ("kind".into(), json!("human.review")),
                    ("reason".into(), json!(format!("change {i}"))),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        if i == 1 {
            historical = store.index().unwrap();
        }
    }
    for cut in [historical, store.index().unwrap()] {
        let status = store.status_for_subject_names_at(
            BTreeSet::from([subject.into()]),
            cut,
            true,
            ClaimsReduction::Full,
        )
        .unwrap();
        let actual = &status
            .subjects
            .iter()
            .find(|row| row.subject == subject)
            .unwrap()
            .claims;
        let expected = full_canonical_claim_id_oracle(&store.readers.get(), subject, Some(cut));
        assert_eq!(actual, &expected);
        assert_eq!(actual.last(), expected.last());
    }
}

/// `count` claims for one subject over a handful of writers, batches and replica
/// positions, with accepted times drawn from a set far smaller than the claim count so
/// canonical tie groups appear everywhere, including across the session-dating window.
fn seeded_claim_summary_fixture(connection: &Connection, seed: u64, count: usize) {
    let mut create = String::from(
        "CREATE TABLE batches(id TEXT PRIMARY KEY, origin TEXT, replica_sequence INTEGER);
             CREATE TABLE claims(id TEXT PRIMARY KEY, subject TEXT, kind TEXT, batch_id TEXT,
                                 store_index INTEGER, accepted_at_unix_ms TEXT);
             CREATE INDEX claims_subject_accepted_index ON claims
                 (subject, length(accepted_at_unix_ms), accepted_at_unix_ms);
             CREATE TABLE replica_records(claim_id TEXT, position INTEGER);",
    );
    for writer in 0..3 {
        create.push_str(&format!(
            "INSERT INTO batches VALUES ('batch-{writer}','writer-{writer}',{writer});"
        ));
    }
    let mut state = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    let mut next = || {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        state >> 33
    };
    let times = [
        "9", "10", "11", "99", "100", "1000",
        "340282366920938463463374607431768211455",
    ];
    for index in 0..count {
        let time = times[(next() % times.len() as u64) as usize];
        let batch = format!("batch-{}", next() % 3);
        create.push_str(&format!(
            "INSERT INTO claims VALUES ('claim-{index:06}','agent/target','runtime.observed','{batch}',{},'{time}');",
            index + 1
        ));
        if next() % 2 == 0 {
            create.push_str(&format!(
                "INSERT INTO replica_records VALUES ('claim-{index:06}',{});",
                next() % 7
            ));
        }
    }
    // Claims of other subjects must never leak into the subject's answers.
    create.push_str(
        "INSERT INTO claims VALUES ('elsewhere','agent/other','runtime.observed','batch-0',0,'10');",
    );
    connection.execute_batch(&create).unwrap();
}

#[test]
fn claims_summary_equals_the_full_canonical_vector_at_every_cut() {
    for seed in 0..24 {
        let counts = [0, 1, 2, 7, 40];
        for count in counts {
            let connection = Connection::open_in_memory().unwrap();
            seeded_claim_summary_fixture(&connection, seed, count);
            for cut in [None, Some(1), Some(count as u64 / 2), Some(count as u64)] {
                let full = full_canonical_claim_id_oracle(&connection, "agent/target", cut);
                assert_eq!(
                    claim_ids_at(&connection, "agent/target", cut).unwrap(),
                    full,
                    "seed {seed}, count {count}"
                );
                let summary =
                    claims_summary_at(&connection, "agent/target", cut).unwrap();
                assert_eq!(
                    summary.latest,
                    full.last().cloned(),
                    "latest, seed {seed}, count {count}"
                );
                assert_eq!(
                    summary.first,
                    full.first().cloned(),
                    "first, seed {seed}, count {count}"
                );
                assert_eq!(
                    summary.count,
                    full.len(),
                    "count, seed {seed}, count {count}"
                );
                for window in 1..=(count + 2) {
                    assert_eq!(
                        window_boundary_at(
                            &connection,
                            "agent/target",
                            cut.unwrap_or(i64::MAX as u64),
                            window,
                        )
                        .unwrap(),
                        (full.len() >= window)
                            .then(|| full[window - 1].clone()),
                        "window {window}, seed {seed}, count {count}"
                    );
                }
            }
        }
    }
}

#[test]
fn claims_summary_resolves_the_real_session_window_inside_a_crossing_tie() {
    let connection = Connection::open_in_memory().unwrap();
    // The window edge lands inside a wide tie group: only canonical ordering decides the
    // boundary, exactly what the full vector resolves claim by claim.
    let mut fixture = String::from(
        "CREATE TABLE batches(id TEXT PRIMARY KEY, origin TEXT, replica_sequence INTEGER);
             CREATE TABLE claims(id TEXT PRIMARY KEY, subject TEXT, kind TEXT, batch_id TEXT,
                                 store_index INTEGER, accepted_at_unix_ms TEXT);
             CREATE INDEX claims_subject_accepted_index ON claims
                 (subject, length(accepted_at_unix_ms), accepted_at_unix_ms);
             CREATE TABLE replica_records(claim_id TEXT, position INTEGER);
             INSERT INTO batches VALUES ('a','writer-a',1),('b','writer-b',2);",
    );
    for index in 0..(SESSION_CLAIMS + 4) {
        // One claim before the window, a tie group spanning it, one claim after.
        let (time, batch) = if index < SESSION_CLAIMS - 3 {
            ("10", "a")
        } else if index < SESSION_CLAIMS + 2 {
            ("12", if index % 2 == 0 { "a" } else { "b" })
        } else {
            ("340282366920938463463374607431768211455", "a")
        };
        fixture.push_str(&format!(
            "INSERT INTO claims VALUES ('claim-{index:06}','agent/target','runtime.observed','{batch}',{},'{time}');
                 INSERT INTO replica_records VALUES ('claim-{index:06}',{});",
            index + 1,
            (index % 5) as u64
        ));
    }
    connection.execute_batch(&fixture).unwrap();
    let full = full_canonical_claim_id_oracle(&connection, "agent/target", None);
    assert_eq!(full.len(), SESSION_CLAIMS + 4);
    let summary = claims_summary_at(&connection, "agent/target", None).unwrap();
    assert_eq!(summary.count, full.len());
    assert_eq!(summary.latest, full.last().cloned());
    assert_eq!(
        summary.session_boundary,
        Some(full[SESSION_CLAIMS - 1].clone()),
        "the boundary is the window's own canonical id, not a physical neighbor"
    );
}

#[test]
fn summary_status_matches_full_status_for_every_internal_decision() {
    let store = Store::open_memory("node").unwrap();
    let subject = "agent/node.summary";
    let mut historical = 0;
    for round in 0..6 {
        store
            .append_claim(&ClaimInput {
                subject: subject.into(),
                kind: "runtime.observed".into(),
                actor: Some("person/avery".into()),
                fields: BTreeMap::from([(
                    "status".into(),
                    json!(if round % 2 == 0 { "running" } else { "waiting" }),
                )]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        if round == 2 {
            historical = store.index().unwrap();
        }
    }
    for cut in [historical, store.index().unwrap()] {
        let full = store
            .status_for_subject_names_at(
                BTreeSet::from([subject.into()]),
                cut,
                true,
                ClaimsReduction::Full,
            )
            .unwrap()
            .subjects
            .remove(0);
        let summary = store
            .status_for_subject_names_at(
                BTreeSet::from([subject.into()]),
                cut,
                true,
                ClaimsReduction::Summary,
            )
            .unwrap()
            .subjects
            .remove(0);
        assert!(
            summary.claims.is_empty(),
            "a summary reduction carries no vector"
        );
        // The agent card's fallback revision.
        assert_eq!(
            summary
                .desired_revision
                .clone()
                .or_else(|| summary.claims_summary.latest.clone()),
            full.desired_revision
                .clone()
                .or_else(|| full.claims.last().cloned())
        );
        // The session list's dating boundary, including its gate on the claim count.
        assert_eq!(
            summary.claims_summary.session_boundary,
            (full.claims.len() > SESSION_CLAIMS)
                .then(|| full.claims[SESSION_CLAIMS - 1].clone())
        );
        assert_eq!(summary.claims_summary.count, full.claims.len());
        assert_eq!(
            summary.claims_summary.latest,
            full.claims.last().cloned()
        );
    }
}

#[test]
fn full_status_stays_full_after_summary_reductions_share_the_caches() {
    let store = Store::open_memory("node").unwrap();
    let subject = "agent/node.caches";
    store
        .append_claim(&ClaimInput {
            subject: subject.into(),
            kind: "runtime.observed".into(),
            actor: Some("person/avery".into()),
            fields: BTreeMap::from([("status".into(), json!("running"))]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    let index = store.index().unwrap();
    let summarized = store
        .status_for_subject_prefix_at(
            "agent/",
            Some(index),
            true,
            ClaimsReduction::Summary,
        )
        .unwrap()
        .subjects
        .remove(0);
    assert!(summarized.claims.is_empty());
    // The public reduction still carries the whole vector at the same index, through the
    // caches the summary reduction just shared.
    let public = store.status_at(Some(subject), None, Some(index)).unwrap();
    let public = public.subjects.iter().find(|row| row.subject == subject).unwrap();
    assert_eq!(
        public.claims,
        full_canonical_claim_id_oracle(&store.readers.get(), subject, Some(index))
    );
    SUBJECT_REDUCTIONS.with(|count| count.set(0));
    let back = store
        .status_for_subject_prefix_at(
            "agent/",
            Some(index),
            true,
            ClaimsReduction::Summary,
        )
        .unwrap()
        .subjects
        .remove(0);
    assert!(back.claims.is_empty());
    assert_eq!(back.claims_summary.latest, public.claims.last().cloned());
    assert_eq!(SUBJECT_REDUCTIONS.with(std::cell::Cell::get), 0);
    store.status_at(Some(subject), None, Some(index)).unwrap();
    assert_eq!(SUBJECT_REDUCTIONS.with(std::cell::Cell::get), 0);
}

#[test]
fn forgetting_views_invalidates_every_status_reduction_mode() {
    let store = Store::open_memory("node").unwrap();
    observe_runtime(&store, "agent/cache/forget", "running", "one");
    let cut = store.index().unwrap();
    let modes = [
        ClaimsReduction::Full,
        ClaimsReduction::Summary,
        ClaimsReduction::Runtime,
        ClaimsReduction::AgentCard,
    ];
    let read = |mode| {
        store
            .status_for_subject_prefix_at("agent/", Some(cut), true, mode)
            .unwrap()
    };
    for mode in modes {
        read(mode);
    }
    SUBJECT_REDUCTIONS.with(|count| count.set(0));
    for mode in modes {
        read(mode);
    }
    assert_eq!(SUBJECT_REDUCTIONS.with(std::cell::Cell::get), 0);
    store.forget_current_views();
    for mode in modes {
        SUBJECT_REDUCTIONS.with(|count| count.set(0));
        read(mode);
        assert_eq!(SUBJECT_REDUCTIONS.with(std::cell::Cell::get), 1, "{mode:?}");
    }
}


/// Run against a reflink-copied live store only:
/// `CLAIM_SUMMARY_BENCH=/path/to/copy.sqlite3 cargo test -p st3 --lib claim_summary -- --ignored --nocapture`.
/// Alternates full and summary reductions of the same subject at the same cut so both
/// answers measure one store, one index and one page cache.
#[test]
#[ignore = "points at a copied live store through CLAIM_SUMMARY_BENCH"]
fn claim_summary_bench_subject_status_at_on_a_copied_store() {
    let path = std::env::var("CLAIM_SUMMARY_BENCH").unwrap_or_default();
    if path.is_empty() {
        return;
    }
    let store = Store::open(std::path::Path::new(&path), "bench-node").unwrap();
    let index = store.index().unwrap();
    let subjects = std::env::var("CLAIM_SUMMARY_BENCH_SUBJECTS")
        .unwrap_or_else(|_| {
            "agent/dotfiles/manager,agent/dotfiles/deployer,agent/dotfiles/smalltalk-graph,daemon/mbp2025".into()
        })
        .split(',')
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let iterations = 30;
    let percentile = |samples: &mut Vec<f64>, fraction: f64| {
        samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
        samples[((samples.len() as f64 - 1.0) * fraction) as usize]
    };
    println!("subject\tclaims\tfull p50 ms\tfull p95 ms\tsummary p50 ms\tsummary p95 ms");
    for subject in subjects {
        let connection = store.readers.get();
        let claims = connection
            .query_row(
                "SELECT COUNT(*) FROM claims WHERE subject=?1",
                [&subject],
                |row| row.get::<_, i64>(0),
            )
            .unwrap();
        let mut full = Vec::new();
        let mut summary = Vec::new();
        for round in 0..iterations {
            let start = std::time::Instant::now();
            let (status, _) = subject_status_at(
                &connection,
                &subject,
                Some(index),
                None,
                ClaimsReduction::Full,
            )
            .unwrap()
            .expect("the live subject reduces");
            let full_ms = start.elapsed().as_secs_f64() * 1e3;
            let start = std::time::Instant::now();
            let (summarized, _) = subject_status_at(
                &connection,
                &subject,
                Some(index),
                None,
                ClaimsReduction::Summary,
            )
            .unwrap()
            .expect("the live subject reduces");
            let summary_ms = start.elapsed().as_secs_f64() * 1e3;
            if round > 0 {
                full.push(full_ms);
                summary.push(summary_ms);
            }
            // Both answers must agree where internal readers look.
            assert_eq!(summarized.claims_summary.first, status.claims.first().cloned());
            assert_eq!(summarized.claims_summary.latest, status.claims.last().cloned());
            assert_eq!(summarized.claims_summary.count, status.claims.len());
            assert_eq!(
                summarized.claims_summary.session_boundary,
                (status.claims.len() > SESSION_CLAIMS)
                    .then(|| status.claims[SESSION_CLAIMS - 1].clone())
            );
        }
        println!(
            "{}\t{}\t{:.1}\t{:.1}\t{:.1}\t{:.1}",
            subject,
            claims,
            percentile(&mut full, 0.5),
            percentile(&mut full, 0.95),
            percentile(&mut summary, 0.5),
            percentile(&mut summary, 0.95)
        );
    }
}

#[test]
fn admission_failure_fences_ready_work_and_exit_until_a_new_incarnation() {
    let store = Store::open_memory("node").unwrap();
    let subject = "agent/node.fixture";
    let append = |kind: &str, fields: BTreeMap<String, Value>| {
        store
            .append_claim(&ClaimInput {
                subject: subject.into(),
                kind: kind.into(),
                actor: Some(subject.into()),
                fields,
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap()
    };
    append(
        "runtime.observed",
        BTreeMap::from([
            ("status".into(), json!("running")),
            ("runtime_id".into(), json!("fixture")),
            ("incarnation_id".into(), json!("first")),
        ]),
    );
    append(
        "harness.observed",
        BTreeMap::from([
            ("state".into(), json!("ready")),
            ("incarnation_id".into(), json!("first")),
        ]),
    );
    assert!(store.current_harness(subject).unwrap().unwrap().is_ready());
    let refusal = append(
        "harness.diagnostic",
        BTreeMap::from([
            ("code".into(), json!("harness-admission-failed")),
            ("status".into(), json!("degraded")),
            (
                "reason".into(),
                json!("opencode 99.42.7 admission failed at admissionLifecycle"),
            ),
            ("incarnation_id".into(), json!("first")),
        ]),
    );
    append(
        "harness.observed",
        BTreeMap::from([
            ("state".into(), json!("working")),
            ("incarnation_id".into(), json!("first")),
        ]),
    );
    let refused = store.current_harness(subject).unwrap().unwrap();
    assert!(!refused.is_ready());
    assert_eq!(refused.state, "indeterminate");
    assert_eq!(refused.since_unix_ms, refusal.accepted_at_unix_ms);
    assert!(refused.reason.unwrap().contains("admissionLifecycle"));
    append(
        "runtime.observed",
        BTreeMap::from([
            ("status".into(), json!("exited")),
            ("incarnation_id".into(), json!("first")),
        ]),
    );
    let exited = store.current_harness(subject).unwrap().unwrap();
    assert!(exited.reason.unwrap().contains("admissionLifecycle"));
    assert_eq!(exited.since_unix_ms, refusal.accepted_at_unix_ms);
    append(
        "runtime.observed",
        BTreeMap::from([
            ("status".into(), json!("running")),
            ("runtime_id".into(), json!("fixture")),
            ("incarnation_id".into(), json!("second")),
        ]),
    );
    append(
        "harness.observed",
        BTreeMap::from([
            ("state".into(), json!("ready")),
            ("incarnation_id".into(), json!("second")),
        ]),
    );
    assert!(store.current_harness(subject).unwrap().unwrap().is_ready());
}

#[test]
fn expired_claude_login_fences_later_ready_and_work_claims_in_same_incarnation() {
    let store = Store::open_memory("node").unwrap();
    let subject = "agent/node.claude";
    let append = |kind: &str, actor: Option<&str>, fields: BTreeMap<String, Value>| {
        store
            .append_claim(&ClaimInput {
                subject: subject.into(),
                kind: kind.into(),
                actor: actor.map(str::to_owned),
                fields,
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
    };
    append(
        "runtime.observed",
        None,
        BTreeMap::from([
            ("status".into(), Value::String("running".into())),
            ("runtime_id".into(), Value::String("node.claude".into())),
            ("incarnation_id".into(), Value::String("first".into())),
        ]),
    );
    append(
        "harness.observed",
        Some(subject),
        BTreeMap::from([
            ("state".into(), Value::String("ready".into())),
            ("incarnation_id".into(), Value::String("first".into())),
        ]),
    );
    assert!(store.current_harness(subject).unwrap().unwrap().is_ready());
    append(
        "harness.diagnostic",
        Some(subject),
        BTreeMap::from([
            ("status".into(), Value::String("unauthenticated".into())),
            ("code".into(), Value::String("provider-auth-expired".into())),
            ("incarnation_id".into(), Value::String("first".into())),
        ]),
    );
    append(
        "harness.observed",
        Some(subject),
        BTreeMap::from([
            ("state".into(), Value::String("ready".into())),
            ("incarnation_id".into(), Value::String("first".into())),
        ]),
    );
    let blocked = store.current_harness(subject).unwrap().unwrap();
    assert_eq!(blocked.state, "needs-login");
    assert_eq!(blocked.reason.as_deref(), Some("providerAuth"));
    assert!(!blocked.is_ready());
    append(
        "runtime.observed",
        None,
        BTreeMap::from([
            ("status".into(), Value::String("running".into())),
            ("runtime_id".into(), Value::String("node.claude".into())),
            ("incarnation_id".into(), Value::String("second".into())),
        ]),
    );
    append(
        "harness.observed",
        Some(subject),
        BTreeMap::from([
            ("state".into(), Value::String("ready".into())),
            ("incarnation_id".into(), Value::String("second".into())),
        ]),
    );
    assert!(store.current_harness(subject).unwrap().unwrap().is_ready());
}

#[test]
fn a_claude_trust_prompt_fences_its_incarnation_as_not_ready_with_the_reason() {
    let store = Store::open_memory("node").unwrap();
    let subject = "agent/node.claude";
    let append = |kind: &str, actor: Option<&str>, fields: BTreeMap<String, Value>| {
        store
            .append_claim(&ClaimInput {
                subject: subject.into(),
                kind: kind.into(),
                actor: actor.map(str::to_owned),
                fields,
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
    };
    let running = |incarnation: &str| {
        BTreeMap::from([
            ("status".into(), Value::String("running".into())),
            ("runtime_id".into(), Value::String("node.claude".into())),
            ("incarnation_id".into(), Value::String(incarnation.into())),
        ])
    };
    let ready = |incarnation: &str| {
        BTreeMap::from([
            ("state".into(), Value::String("ready".into())),
            ("incarnation_id".into(), Value::String(incarnation.into())),
        ])
    };
    append("runtime.observed", None, running("first"));
    append(
        "harness.diagnostic",
        Some(subject),
        BTreeMap::from([
            ("status".into(), Value::String("blocked".into())),
            ("code".into(), Value::String("provider-trust-prompt".into())),
            ("incarnation_id".into(), Value::String("first".into())),
        ]),
    );
    // A channel that initializes behind the prompt must not make the seat look ready.
    append("harness.observed", Some(subject), ready("first"));
    let blocked = store.current_harness(subject).unwrap().unwrap();
    assert_eq!(blocked.state, "blocked");
    assert_eq!(blocked.reason.as_deref(), Some("providerTrustPrompt"));
    assert!(!blocked.is_ready());

    append("runtime.observed", None, running("second"));
    append("harness.observed", Some(subject), ready("second"));
    assert!(store.current_harness(subject).unwrap().unwrap().is_ready());
}
