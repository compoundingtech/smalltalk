#[test]
fn deadline_upgrade_preserves_budgets_admission_and_other_projection_digests() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.sqlite3");
    let store = Store::open(&path, "node").unwrap();
    let mission = publish_mission(
        &store,
        r#"version 2
mission "budget" state="ready" timeout="5m" {
  concurrent-runs
  goal "Keep a timeout budget across admission."
  step "work" { agentless }
}
"#,
        "deadline-upgrade-mission",
    );
    let immediate = store
        .create_mission_run(&MissionRunRequest {
            mission: "budget".into(),
            revision: None,
            workspace: "/tmp".into(),
            requester: Some("person/operator".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "deadline-immediate".into(),
        })
        .unwrap();
    let mut waiters = BTreeMap::new();
    for id in ["queued", "admitted", "cancelled", "missing-admission"] {
        let source = format!(
            "version 2\nmission-run {id:?} {{\n  mission \"mission/budget@{}\"\n  workspace \"/tmp\"\n  requester \"person/operator\"\n  after {:?}\n}}\n",
            mission.revision, immediate.subject,
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
            .apply(&intent, &preview.subject_tokens, &format!("start-{id}"))
            .unwrap();
        waiters.insert(id, store.mission_run(id).unwrap().unwrap());
    }
    let admitted_at = waiters["admitted"].created_at_unix_ms + 600_000;
    struct Clock;
    impl Drop for Clock {
        fn drop(&mut self) {
            smallclaims::store::set_thread_clock(None);
        }
    }
    let clock = Clock;
    smallclaims::store::set_thread_clock(Some(admitted_at));
    store.set_write_clock_at(admitted_at).unwrap();
    let after = waiters["admitted"]
        .steps
        .iter()
        .find(|step| step.step == "after-run")
        .unwrap();
    store
        .set_step_state(&after.subject, "completed", None)
        .unwrap();
    let missing_after = waiters["missing-admission"]
        .steps
        .iter()
        .find(|step| step.step == "after-run")
        .unwrap();
    store
        .set_step_state(&missing_after.subject, "completed", None)
        .unwrap();
    let missing_claim = store
        .latest_claim(&missing_after.subject, Some("step-run.state"))
        .unwrap()
        .unwrap();
    // Retention/checkpoint trimming can remove admission history while preserving
    // the completed after-step projection. It must never silently become a waiter.
    store
        .connection
        .write()
        .execute(
            "DELETE FROM claims WHERE subject=?1 AND kind='step-run.state'
         AND json_extract(body, '$.fields.status')='completed'",
            [&missing_after.subject],
        )
        .unwrap();
    let admitted = store.mission_run("admitted").unwrap().unwrap();
    let work = admitted
        .steps
        .iter()
        .find(|step| step.step == "work")
        .unwrap();
    store
        .set_step_state(&work.subject, "failed", Some("retry the work"))
        .unwrap();
    store
        .set_mission_run_state("admitted", "failed", "terminal", Some("work failed"))
        .unwrap();
    store
        .retry_failed_step(
            &work.subject,
            "person/operator",
            "try again",
            "retry-admitted-work",
        )
        .unwrap();
    let retried = store.mission_run("admitted").unwrap().unwrap();
    assert_ne!(retried.generation, admitted.generation);
    assert_eq!(retried.deadline_at_unix_ms, Some(admitted_at + 300_000));
    store
        .request_mission_run_cancellation("cancelled", "not needed")
        .unwrap();
    let index = store.index().unwrap();
    let other_digests;
    {
        // Install the old table and creation-based clocks, with its normal triggers/digest.
        let connection = store.connection.write();
        let triggers = connection.prepare(
            "SELECT sql FROM sqlite_master WHERE type='trigger' AND tbl_name='mission_run_deadlines'",
        ).unwrap().query_map([], |row| row.get::<_, String>(0)).unwrap()
            .collect::<rusqlite::Result<Vec<_>>>().unwrap();
        connection
            .execute_batch(
                "DROP TABLE mission_run_deadlines;
             CREATE TABLE mission_run_deadlines (
                 run_id TEXT PRIMARY KEY REFERENCES mission_runs(id),
                 timeout_ms INTEGER NOT NULL,
                 deadline_at_unix_ms TEXT NOT NULL
             );
             UPDATE projection_digest_state SET row_count=0, accumulator=zeroblob(64)
             WHERE table_name='mission_run_deadlines';",
            )
            .unwrap();
        for sql in triggers {
            connection.execute_batch(&sql).unwrap();
        }
        connection
            .execute(
                "INSERT INTO mission_run_deadlines
             SELECT id, 300000, CAST(CAST(created_at_unix_ms AS INTEGER)+300000 AS TEXT)
             FROM mission_runs",
                [],
            )
            .unwrap();
        let cached = smallclaims::store::projection_digest::tables(&connection).unwrap();
        assert_eq!(cached, projection_digest::oracle(&connection).unwrap());
        other_digests = cached
            .into_iter()
            .filter(|(table, _)| table != "mission_run_deadlines")
            .collect::<BTreeMap<_, _>>();
    }
    drop(clock);
    drop(store);
    let upgraded = Store::open(&path, "node").unwrap();
    assert_eq!(upgraded.index().unwrap(), index);
    for id in ["queued", "cancelled"] {
        let run = upgraded.mission_run(id).unwrap().unwrap();
        assert_eq!(run.timeout_ms, Some(300_000));
        assert_eq!(run.deadline_at_unix_ms, None, "{id}");
    }
    let missing = upgraded.mission_run("missing-admission").unwrap().unwrap();
    assert_eq!(missing.timeout_ms, Some(300_000));
    assert_eq!(missing.deadline_at_unix_ms, None);
    let faults = upgraded.fault_snapshot(now_ms()).unwrap();
    assert!(
        faults
            .iter()
            .any(|fault| fault.item.subject == missing.subject
                && fault.item.title == "Run admission time unavailable"),
        "an admitted run with trimmed completion history needs an explicit fault"
    );
    assert!(
        !faults
            .iter()
            .any(|fault| fault.item.subject == waiters["queued"].subject
                && fault.item.title == "Run admission time unavailable"),
        "a genuinely queued run is not missing its admission time"
    );
    assert_eq!(
        upgraded
            .mission_run("admitted")
            .unwrap()
            .unwrap()
            .deadline_at_unix_ms,
        Some(admitted_at + 300_000)
    );
    assert_eq!(
        upgraded
            .mission_run(&immediate.id)
            .unwrap()
            .unwrap()
            .deadline_at_unix_ms,
        immediate.deadline_at_unix_ms
    );
    {
        let connection = upgraded.connection.write();
        let cached = smallclaims::store::projection_digest::tables(&connection).unwrap();
        assert_eq!(cached, projection_digest::oracle(&connection).unwrap());
        assert_eq!(
            cached
                .into_iter()
                .filter(|(table, _)| table != "mission_run_deadlines")
                .collect::<BTreeMap<_, _>>(),
            other_digests
        );
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
                    row.get::<_, u64>(0)
                })
                .unwrap(),
            0
        );
    }
    // A duplicate completion and a later generation cannot invent a replacement
    // clock while the first admission is unknown.
    let later_clock = Clock;
    smallclaims::store::set_thread_clock(Some(admitted_at + 1_800_000));
    upgraded
        .set_write_clock_at(admitted_at + 1_800_000)
        .unwrap();
    let completion = |subject: &str, key: &str| ClaimInput {
        subject: subject.into(),
        kind: "step-run.state".into(),
        actor: None,
        fields: BTreeMap::from([("status".into(), json!("completed"))]),
        evidence: Vec::new(),
        expected_subject: None,
        idempotency_key: Some(key.into()),
    };
    upgraded
        .append_claim(&completion(
            &missing_after.subject,
            "duplicate-unknown-admission",
        ))
        .unwrap();
    assert_eq!(
        upgraded
            .mission_run("missing-admission")
            .unwrap()
            .unwrap()
            .deadline_at_unix_ms,
        None
    );
    let missing_work = missing
        .steps
        .iter()
        .find(|step| step.step == "work")
        .unwrap();
    upgraded
        .set_step_state(&missing_work.subject, "failed", Some("retry work"))
        .unwrap();
    upgraded
        .set_mission_run_state(
            "missing-admission",
            "failed",
            "terminal",
            Some("work failed"),
        )
        .unwrap();
    upgraded
        .retry_failed_step(
            &missing_work.subject,
            "person/operator",
            "try again",
            "retry-unknown-clock",
        )
        .unwrap();
    let retried_missing = upgraded.mission_run("missing-admission").unwrap().unwrap();
    assert_ne!(retried_missing.generation, missing.generation);
    let next_after = retried_missing
        .steps
        .iter()
        .find(|step| step.step == "after-run")
        .unwrap();
    upgraded
        .append_claim(&completion(
            &next_after.subject,
            "next-generation-unknown-admission",
        ))
        .unwrap();
    assert_eq!(
        upgraded
            .mission_run("missing-admission")
            .unwrap()
            .unwrap()
            .deadline_at_unix_ms,
        None
    );
    assert_eq!(
        upgraded
            .fault_snapshot(now_ms())
            .unwrap()
            .iter()
            .filter(|fault| fault.item.subject == missing.subject
                && fault.item.title == "Run admission time unavailable")
            .count(),
        1
    );
    // Restore the exact original retained record, rather than manufacture an admission.
    upgraded.connection.write().execute(
        "INSERT INTO claims(store_index,id,batch_id,subject,kind,origin,actor,body,predecessors,accepted_at_unix_ms)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
        params![missing_claim.store_index, missing_claim.id, missing_claim.batch_id, missing_claim.subject,
            missing_claim.kind, missing_claim.origin, missing_claim.actor,
            serde_json::to_string(&missing_claim.body).unwrap(), serde_json::to_string(&missing_claim.predecessors).unwrap(),
            missing_claim.accepted_at_unix_ms.to_string()],
    ).unwrap();
    // Replaying the actual graph, rather than only auxiliary projections, preserves the
    // same accepted admission clock that the migration recovered from historical claims.
    {
        let mut connection = upgraded.connection.write();
        let transaction = connection.transaction().unwrap();
        replay_graph_from_nothing_tx(&transaction).unwrap();
        assert_eq!(
            mission_run_view_tx(&transaction, "admitted")
                .unwrap()
                .deadline_at_unix_ms,
            Some(admitted_at + 300_000)
        );
        assert_eq!(
            mission_run_view_tx(&transaction, "missing-admission")
                .unwrap()
                .deadline_at_unix_ms,
            Some(admitted_at + 300_000)
        );
        transaction.commit().unwrap();
    }
    assert!(
        !upgraded
            .fault_snapshot(now_ms())
            .unwrap()
            .iter()
            .any(|fault| fault.item.subject == missing.subject
                && fault.item.title == "Run admission time unavailable"),
        "restored admission history clears the fault"
    );
    drop(later_clock);
    // Reopening again neither changes an admitted clock nor repeats migration.
    drop(upgraded);
    let reopened = Store::open(&path, "node").unwrap();
    assert_eq!(
        reopened
            .mission_run("admitted")
            .unwrap()
            .unwrap()
            .deadline_at_unix_ms,
        Some(admitted_at + 300_000)
    );
}

#[test]
fn a_corrupt_admission_stamp_stops_open_without_a_partial_deadline_migration() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("corrupt.sqlite3");
    let store = Store::open(&path, "node").unwrap();
    let mission = publish_mission(
        &store,
        r#"version 2
mission "corrupt-clock" state="ready" timeout="5m" {
  concurrent-runs
  goal "Keep corrupt admission history explicit."
  step "work" { agentless }
}
"#,
        "corrupt-clock-mission",
    );
    let root = store
        .create_mission_run(&MissionRunRequest {
            mission: "corrupt-clock".into(),
            revision: None,
            workspace: "/tmp".into(),
            requester: Some("person/operator".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "corrupt-root".into(),
        })
        .unwrap();
    let source = format!(
        "version 2\nmission-run \"corrupt-waiter\" {{ mission \"mission/corrupt-clock@{}\"; workspace \"/tmp\"; requester \"person/operator\"; after {:?} }}",
        mission.revision, root.subject
    );
    let intent = parse_intent(&source, "node").unwrap();
    let plan = store
        .mission(
            &intent,
            IntentInput {
                kdl: source,
                source_name: None,
            },
        )
        .unwrap();
    store
        .apply(&intent, &plan.subject_tokens, "corrupt-waiter")
        .unwrap();
    let waiter = store.mission_run("corrupt-waiter").unwrap().unwrap();
    let after = waiter
        .steps
        .iter()
        .find(|step| step.step == "after-run")
        .unwrap();
    store
        .set_step_state(&after.subject, "completed", None)
        .unwrap();
    {
        let connection = store.connection.write();
        let triggers = connection.prepare("SELECT sql FROM sqlite_master WHERE type='trigger' AND tbl_name='mission_run_deadlines'")
            .unwrap().query_map([], |row| row.get::<_, String>(0)).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap();
        let rows = connection
            .prepare("SELECT run_id,timeout_ms,deadline_at_unix_ms FROM mission_run_deadlines")
            .unwrap()
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, u64>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        connection.execute_batch("DROP TABLE mission_run_deadlines;
            CREATE TABLE mission_run_deadlines(run_id TEXT PRIMARY KEY REFERENCES mission_runs(id), timeout_ms INTEGER NOT NULL, deadline_at_unix_ms TEXT NOT NULL);
            UPDATE projection_digest_state SET row_count=0, accumulator=zeroblob(64) WHERE table_name='mission_run_deadlines';").unwrap();
        for sql in triggers {
            connection.execute_batch(&sql).unwrap();
        }
        for (run, timeout, deadline) in rows {
            connection
                .execute(
                    "INSERT INTO mission_run_deadlines VALUES(?1,?2,?3)",
                    params![run, timeout, deadline],
                )
                .unwrap();
        }
        // Invalid acceptance data cannot be admitted through the typed claim API.
        // Model on-disk corruption, not a legitimate retained-history gap.
        connection.execute("UPDATE claims SET accepted_at_unix_ms='broken-clock' WHERE subject=?1 AND kind='step-run.state' AND json_extract(body,'$.fields.status')='completed'", [&after.subject]).unwrap();
    }
    drop(store);
    let error = Store::open(&path, "node")
        .err()
        .expect("corrupt acceptance must stop open")
        .to_string();
    assert!(
        error.contains(
            "invalid accepted after-run completion timestamp for mission-run/corrupt-waiter"
        ),
        "{error}"
    );
    assert!(error.contains("broken-clock"), "{error}");
    let connection = Connection::open(&path).unwrap();
    assert!(connection.query_row("SELECT \"notnull\" FROM pragma_table_info('mission_run_deadlines') WHERE name='deadline_at_unix_ms'", [], |row| row.get::<_, bool>(0)).unwrap());
    assert_eq!(
        smallclaims::store::projection_digest::tables(&connection).unwrap(),
        projection_digest::oracle(&connection).unwrap(),
        "failed migration preserves the previous table and digest triggers"
    );
}

#[test]
fn an_old_creation_claim_cannot_arm_a_waiting_deadline_on_replay() {
    let store = Store::open_memory("node").unwrap();
    let mission = publish_mission(
        &store,
        r#"version 2
mission "budget" state="ready" timeout="5m" {
  concurrent-runs
  goal "Rebuild a historical waiting deadline."
  step "work" { agentless }
}
"#,
        "legacy-deadline-mission",
    );
    let predecessor = store
        .create_mission_run(&MissionRunRequest {
            mission: "budget".into(),
            revision: None,
            workspace: "/tmp".into(),
            requester: Some("person/operator".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "legacy-predecessor".into(),
        })
        .unwrap();
    let source = format!(
        "version 2\nmission-run \"legacy-waiter\" {{\n  mission \"mission/budget@{}\"\n  workspace \"/tmp\"\n  requester \"person/operator\"\n  after {:?}\n}}\n",
        mission.revision, predecessor.subject,
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
        .apply(&intent, &preview.subject_tokens, "legacy-waiter")
        .unwrap();
    let mut claim = store
        .latest_claim("mission-run/legacy-waiter", Some("mission-run.created"))
        .unwrap()
        .unwrap();
    claim.body["fields"]["deadline_at_unix_ms"] = json!(claim.accepted_at_unix_ms + 300_000);
    let mut connection = store.connection.write();
    let transaction = connection.transaction().unwrap();
    transaction
        .execute(
            "DELETE FROM mission_run_deadlines WHERE run_id='legacy-waiter'",
            [],
        )
        .unwrap();
    for table in [
        "step_runs",
        "mission_run_after",
        "run_generations",
        "mission_runs",
    ] {
        let key = if table == "mission_runs" {
            "id"
        } else {
            "run_id"
        };
        transaction
            .execute(
                &format!("DELETE FROM {table} WHERE {key}='legacy-waiter'"),
                [],
            )
            .unwrap();
    }
    project_mission_run_created(&transaction, &claim).unwrap();
    let waiting = mission_run_view_tx(&transaction, "legacy-waiter").unwrap();
    assert_eq!(waiting.timeout_ms, Some(300_000));
    assert_eq!(waiting.deadline_at_unix_ms, None);
}

#[test]
#[ignore = "100,000-row startup measurement; run explicitly with --ignored --nocapture"]
fn deadline_migration_startup_at_one_hundred_thousand_rows() {
    measure_deadline_migration_startup(100_000, 20_000, 100_001);
}

#[test]
#[ignore = "Synthetic fixture at the operations census size; run explicitly"]
fn deadline_migration_startup_at_the_operations_census_size() {
    measure_deadline_migration_startup(2_368, 42, 5_458);
}

fn measure_deadline_migration_startup(rows: u64, chained: u64, total_runs: u64) {
    assert!(chained <= rows && rows < total_runs);
    let admitted = chained.div_ceil(2);
    let queued = chained - admitted;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("scale.sqlite3");
    let store = Store::open(&path, "node").unwrap();
    publish_mission(
        &store,
        r#"version 2
mission "scale" state="ready" timeout="5m" {
  concurrent-runs
  goal "Measure admission deadline migration."
  step "work" { agentless }
}
"#,
        "scale-mission",
    );
    let seed = store
        .create_mission_run(&MissionRunRequest {
            mission: "scale".into(),
            revision: None,
            workspace: "/tmp".into(),
            requester: Some("person/operator".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "scale-seed".into(),
        })
        .unwrap();
    {
        // This is a projection-scale fixture, not a claim replay benchmark. Populate before
        // timing, with normal digest/mutation triggers enabled and real foreign-key targets.
        let mut connection = store.connection.write();
        let transaction = connection.transaction().unwrap();
        transaction
            .execute_batch(&format!(
                "CREATE TEMP TABLE fixture_numbers(n INTEGER PRIMARY KEY);
            INSERT INTO fixture_numbers WITH RECURSIVE numbers(n) AS
            (VALUES(0) UNION ALL SELECT n+1 FROM numbers WHERE n<{}) SELECT n FROM numbers;",
                total_runs - 2,
            ))
            .unwrap();
        let clone_table = |table: &str, overrides: &[(&str, &str)], predicate: &str| {
            let columns = transaction
                .prepare(&format!("PRAGMA table_info({table})"))
                .unwrap()
                .query_map([], |row| row.get::<_, String>(1))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            let select = columns
                .iter()
                .map(|column| {
                    overrides
                        .iter()
                        .find(|(key, _)| *key == column)
                        .map(|(_, value)| value.to_string())
                        .unwrap_or_else(|| format!("seed.{column}"))
                })
                .collect::<Vec<_>>()
                .join(",");
            transaction
                .execute(
                    &format!(
                        "INSERT INTO {table}({}) SELECT {select}
                FROM {table} seed CROSS JOIN fixture_numbers WHERE {predicate}",
                        columns.join(",")
                    ),
                    [&seed.id],
                )
                .unwrap();
        };
        clone_table(
            "mission_runs",
            &[
                ("id", "'scale-'||n"),
                ("root_run_id", "'scale-'||n"),
                ("current_generation_id", "'scale-generation-'||n"),
            ],
            "seed.id=?1",
        );
        clone_table(
            "run_generations",
            &[("id", "'scale-generation-'||n"), ("run_id", "'scale-'||n")],
            "seed.run_id=?1",
        );
        clone_table(
            "step_runs",
            &[
                ("subject", "'step-run/scale-after-'||n"),
                ("run_id", "'scale-'||n"),
                ("generation_id", "'scale-generation-'||n"),
                ("step_path", "'after-run'"),
                (
                    "status",
                    "CASE WHEN n%2=0 THEN 'completed' ELSE 'pending' END",
                ),
            ],
            &format!("seed.run_id=?1 AND n<{chained}"),
        );
        transaction
            .execute(
                "INSERT INTO mission_run_after SELECT 'scale-'||n, ?1
            FROM fixture_numbers WHERE n<?2",
                params![seed.subject, chained],
            )
            .unwrap();
        // Half the chained runs have an accepted completion. Its historical claim lookup
        // uses the same indexed subject/kind/status columns as a real after-run completion.
        let batch: String = transaction
            .query_row("SELECT id FROM batches LIMIT 1", [], |row| row.get(0))
            .unwrap();
        transaction.execute("INSERT INTO claims(id,batch_id,subject,kind,origin,actor,body,predecessors,accepted_at_unix_ms)
            SELECT 'scale-claim-'||n,?1,'step-run/scale-after-'||n,'step-run.state','node',NULL,
            '{\"fields\":{\"status\":\"completed\"}}','[]','1800000600000'
            FROM fixture_numbers WHERE n<?2 AND n%2=0", params![batch, chained]).unwrap();
        let triggers = transaction
            .prepare(
                "SELECT sql FROM sqlite_master WHERE type='trigger'
            AND tbl_name='mission_run_deadlines'",
            )
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        transaction
            .execute_batch(
                "DROP TABLE mission_run_deadlines;
            CREATE TABLE mission_run_deadlines(run_id TEXT PRIMARY KEY REFERENCES mission_runs(id),
                timeout_ms INTEGER NOT NULL, deadline_at_unix_ms TEXT NOT NULL);
            UPDATE projection_digest_state SET row_count=0, accumulator=zeroblob(64)
                WHERE table_name='mission_run_deadlines';",
            )
            .unwrap();
        for trigger in triggers {
            transaction.execute_batch(&trigger).unwrap();
        }
        transaction
            .execute(
                "INSERT INTO mission_run_deadlines
            SELECT 'scale-'||n,300000,'1800000300000' FROM fixture_numbers WHERE n<?1",
                [rows],
            )
            .unwrap();
        assert_eq!(
            transaction
                .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
                    row.get::<_, u64>(0)
                })
                .unwrap(),
            0
        );
        transaction.commit().unwrap();
    }
    drop(store);
    let started = std::time::Instant::now();
    let migrated = Store::open(&path, "node").unwrap();
    let elapsed = started.elapsed();
    let connection = migrated.readers.get();
    assert_eq!(
        connection
            .query_row("SELECT COUNT(*) FROM mission_runs", [], |row| row
                .get::<_, u64>(0))
            .unwrap(),
        total_runs
    );
    assert_eq!(
        connection
            .query_row("SELECT COUNT(*) FROM mission_run_after", [], |row| row
                .get::<_, u64>(0))
            .unwrap(),
        chained
    );
    assert_eq!(
        connection
            .query_row("SELECT COUNT(*) FROM mission_run_deadlines", [], |row| row
                .get::<_, u64>(
                0
            ))
            .unwrap(),
        rows
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT COUNT(*) FROM mission_run_deadlines WHERE deadline_at_unix_ms IS NULL",
                [],
                |row| row.get::<_, u64>(0)
            )
            .unwrap(),
        queued
    );
    assert_eq!(connection.query_row("SELECT COUNT(*) FROM mission_run_deadlines WHERE deadline_at_unix_ms='1800000900000'", [],
        |row| row.get::<_, u64>(0)).unwrap(), admitted);
    assert_eq!(
        smallclaims::store::projection_digest::tables(&connection).unwrap(),
        projection_digest::oracle(&connection).unwrap()
    );
    println!(
        "DEADLINE_MIGRATION rows={rows} chained={chained} admitted={admitted} queued={queued} mission_runs={total_runs} synthetic=true startup_ms={}",
        elapsed.as_millis()
    );
    assert!(elapsed < std::time::Duration::from_secs(30), "{elapsed:?}");
}

#[test]
fn deadline_definition_versions_skip_only_incompatible_tables() {
    let (source, target, _) = synced_takeover_pair();
    target.begin_first_sync("source").unwrap();
    let mut legacy = source.export_replication_summary(TEST_FLEET).unwrap();
    assert_eq!(legacy.projection_versions["mission_run_deadlines"], 3);
    assert!(
        legacy
            .projection_digests
            .contains_key("mission_run_deadlines")
    );
    legacy.schema_digest = legacy.projection_registry_digest.take().unwrap();
    legacy.projection_versions.clear();
    legacy
        .projection_digests
        .insert("mission_run_deadlines".into(), "old-creation-clock".into());
    let before = target.export_replication_summary(TEST_FLEET).unwrap();
    let receipt = target
        .receive_replication_exchange_asking("source", TEST_FLEET, &legacy, true)
        .unwrap();
    assert!(serde_json::to_value(receipt).unwrap().get("heal").is_none());
    let status = target
        .replication_status(true, Some(TEST_FLEET), &["source".into()])
        .unwrap();
    assert_eq!(
        status.peers[0].incompatible_projection_tables,
        ["mission_run_deadlines"]
    );
    assert!(!status.peers[0].projection_comparison_waiting);
    assert!(
        status.peers[0]
            .projection_digests
            .contains_key("mission_run_deadlines")
    );
    assert!(status.peers[0].differing_tables.is_empty());
    // Comparable tables still report real differences, while claims sync never repairs them.
    legacy
        .projection_digests
        .insert("mission_runs".into(), "broken-run-projection".into());
    target
        .receive_replication_exchange_asking("source", TEST_FLEET, &legacy, true)
        .unwrap();
    let status = target
        .replication_status(true, Some(TEST_FLEET), &["source".into()])
        .unwrap();
    assert_eq!(status.peers[0].differing_tables, ["mission_runs"]);
    // Once the definitions match, a bad deadline digest is itself a real difference.
    legacy.projection_digests.insert(
        "mission_runs".into(),
        source
            .export_replication_summary(TEST_FLEET)
            .unwrap()
            .projection_digests["mission_runs"]
            .clone(),
    );
    legacy
        .projection_versions
        .insert("mission_run_deadlines".into(), 3);
    target
        .receive_replication_exchange_asking("source", TEST_FLEET, &legacy, true)
        .unwrap();
    let status = target
        .replication_status(true, Some(TEST_FLEET), &["source".into()])
        .unwrap();
    assert!(status.peers[0].incompatible_projection_tables.is_empty());
    assert_eq!(status.peers[0].differing_tables, ["mission_run_deadlines"]);
    assert_eq!(
        target
            .export_replication_summary(TEST_FLEET)
            .unwrap()
            .projection_digests,
        before.projection_digests,
        "claims comparison never repairs local projections"
    );
}
