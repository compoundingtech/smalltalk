//! Synthetic retained-row reader controls; these do not qualify admission or replication.
use super::*;
use rusqlite::StatementStatus;

const LEGACY_QUERY: &str = "SELECT EXISTS(
    SELECT 1 FROM claims AS started
    JOIN mission_runs AS run
      ON json_extract(started.body, '$.fields.mission_run')='mission-run/' || run.id
    WHERE started.subject=?1 AND started.kind='schedule.work-started'
      AND run.status NOT IN ('completed','cancelled','failed')
)";

fn insert_run(connection: &Connection, id: &str, status: &str, phase: &str) {
    connection
        .execute(
            "INSERT INTO mission_runs(id,mission_id,initial_revision,current_generation_id,
             root_revision,root_run_id,workspace,requester,inputs,mode,status,phase,
             created_at_unix_ms,updated_at_unix_ms)
         VALUES(?1,'mission/test','revision','generation','revision',?1,'/tmp/test',
             'person/test','{}','normal',?2,?3,'1','1')",
            params![id, status, phase],
        )
        .unwrap();
}

fn insert_claim(connection: &Connection, id: &str, subject: &str, kind: &str, body: Value) {
    // Deliberately foreign origin: the existing historical-start query has no origin filter.
    connection
        .execute(
            "INSERT INTO batches(id,origin,replica_sequence,hash,accepted_at_unix_ms)
         VALUES(?1,'foreign',1,?1,'1')",
            [id],
        )
        .unwrap();
    connection.execute(
        "INSERT INTO claims(id,batch_id,subject,kind,origin,body,predecessors,accepted_at_unix_ms)
         VALUES(?1,?1,?2,?3,'foreign',?4,'[]','1')",
        params![id, subject, kind, body.to_string()],
    ).unwrap();
}

fn assert_result(store: &Store, subject: &str, expected: bool) {
    let legacy: bool = store
        .readers
        .get()
        .query_row(LEGACY_QUERY, [subject], |row| row.get(0))
        .unwrap();
    assert_eq!(legacy, expected, "legacy fixture: {subject}");
    assert_eq!(
        store.schedule_has_active_started_run(subject).unwrap(),
        expected,
        "{subject}"
    );
}

#[test]
fn schedule_active_run_seek_preserves_reference_type_prefix_and_binary_equality() {
    let store = Store::open_memory("node").unwrap();
    let cases = [
        (
            "plain",
            json!({"fields":{"mission_run":"mission-run/plain"}}),
            true,
        ),
        ("", json!({"fields":{"mission_run":"mission-run/"}}), true),
        (
            "123",
            json!({"fields":{"mission_run":"mission-run/123"}}),
            true,
        ),
        (
            "é/雪",
            json!({"fields":{"mission_run":"mission-run/é/雪"}}),
            true,
        ),
        (
            "nul\0tail",
            json!({"fields":{"mission_run":"mission-run/nul\u{0}tail"}}),
            true,
        ),
        (
            "mission-run/nested",
            json!({"fields":{"mission_run":"mission-run/mission-run/nested"}}),
            true,
        ),
        (
            " space ",
            json!({"fields":{"mission_run":"mission-run/ space "}}),
            true,
        ),
        (
            "%_",
            json!({"fields":{"mission_run":"mission-run/%_"}}),
            true,
        ),
        ("plain", json!({"fields":{}}), false),
        ("plain", json!({"mission_run":"mission-run/plain"}), false),
        ("plain", json!({"fields":{"mission_run":null}}), false),
        ("123", json!({"fields":{"mission_run":123}}), false),
        ("123", json!({"fields":{"mission_run":123.0}}), false),
        ("plain", json!({"fields":{"mission_run":true}}), false),
        ("plain", json!({"fields":{"mission_run":false}}), false),
        (
            "plain",
            json!({"fields":{"mission_run":["mission-run/plain"]}}),
            false,
        ),
        (
            "plain",
            json!({"fields":{"mission_run":{"id":"mission-run/plain"}}}),
            false,
        ),
        ("plain", json!({"fields":{"mission_run":"plain"}}), false),
        (
            "plain",
            json!({"fields":{"mission_run":"foreign-run/plain"}}),
            false,
        ),
        (
            "plain",
            json!({"fields":{"mission_run":"Mission-run/plain"}}),
            false,
        ),
        (
            "plain",
            json!({"fields":{"mission_run":"mission-run/Plain"}}),
            false,
        ),
        (
            "plain",
            json!({"fields":{"mission_run":"mission-run/plain/"}}),
            false,
        ),
        (
            "plain",
            json!({"fields":{"mission_run":"mission-run/plain "}}),
            false,
        ),
        (
            "plain",
            json!({"fields":{"mission_run":"mission-run/missing"}}),
            false,
        ),
        ("plain", json!({"fields":{"mission_run":""}}), false),
    ];
    {
        let writer = store.connection.write();
        for id in [
            "plain",
            "",
            "123",
            "é/雪",
            "nul\0tail",
            "mission-run/nested",
            " space ",
            "%_",
        ] {
            insert_run(&writer, id, "running", "normal");
        }
        for (index, (_, body, _)) in cases.iter().enumerate() {
            insert_claim(
                &writer,
                &format!("reference-{index}"),
                &format!("schedule/{index}"),
                "schedule.work-started",
                body.clone(),
            );
        }
    }
    for (index, (_, _, expected)) in cases.iter().enumerate() {
        assert_result(&store, &format!("schedule/{index}"), *expected);
    }
}

#[test]
fn schedule_active_run_seek_preserves_status_only_and_historical_started_semantics() {
    let store = Store::open_memory("node").unwrap();
    {
        let writer = store.connection.write();
        for (index, status) in [
            "running",
            "paused",
            "pending",
            "review",
            "unknown",
            "Completed",
            "completed",
            "cancelled",
            "failed",
        ]
        .iter()
        .enumerate()
        {
            // Phase alone has never made this query terminal.
            for phase in ["normal", "revision-draining", "terminal"] {
                let id = format!("status-{index}-{phase}");
                insert_run(&writer, &id, status, phase);
                insert_claim(
                    &writer,
                    &id,
                    &format!("schedule/{id}"),
                    "schedule.work-started",
                    json!({"fields":{"mission_run":format!("mission-run/{id}")}}),
                );
            }
        }
        insert_run(&writer, "older", "running", "normal");
        insert_run(&writer, "newer", "completed", "terminal");
        for id in ["older", "newer"] {
            insert_claim(
                &writer,
                &format!("history-{id}"),
                "schedule/history",
                "schedule.work-started",
                json!({"fields":{"request":id,"mission_run":format!("mission-run/{id}")}}),
            );
        }
        insert_claim(
            &writer,
            "later-failure",
            "schedule/history",
            "schedule.work-failed",
            json!({"fields":{"request":"older"}}),
        );
        insert_claim(
            &writer,
            "other-kind",
            "schedule/wrong-kind",
            "schedule.work-requested",
            json!({"fields":{"mission_run":"mission-run/older"}}),
        );
    }
    for index in 0..9 {
        for phase in ["normal", "revision-draining", "terminal"] {
            assert_result(
                &store,
                &format!("schedule/status-{index}-{phase}"),
                index < 6,
            );
        }
    }
    assert_result(&store, "schedule/history", true);
    assert_result(&store, "schedule/wrong-kind", false);
    assert_result(&store, "schedule/other-subject", false);
    store
        .connection
        .write()
        .execute(
            "UPDATE mission_runs SET status='failed' WHERE id='older'",
            [],
        )
        .unwrap();
    assert_result(&store, "schedule/history", false);
    store
        .connection
        .write()
        .execute(
            "UPDATE mission_runs SET status='running',phase='terminal' WHERE id='older'",
            [],
        )
        .unwrap();
    assert_result(&store, "schedule/history", true);
}

#[test]
fn schedule_active_run_seek_keeps_malformed_json_an_error() {
    // Storage-corruption reader control only. Production expression indexes reject this
    // body on insertion, so use minimal tables without those admission/index safeguards.
    let connection = Connection::open_in_memory().unwrap();
    connection
        .execute_batch(
            "CREATE TABLE claims(subject TEXT,kind TEXT,body TEXT);
         CREATE TABLE mission_runs(id TEXT PRIMARY KEY,status TEXT);
         INSERT INTO claims VALUES('schedule/bad','schedule.work-started','{');
         INSERT INTO mission_runs VALUES('run','running');",
        )
        .unwrap();
    for query in [LEGACY_QUERY, Store::SCHEDULE_ACTIVE_STARTED_RUN_QUERY] {
        let result = connection.query_row(query, ["schedule/bad"], |row| row.get::<_, bool>(0));
        assert!(
            result.is_err(),
            "malformed JSON must not become an empty result"
        );
    }
}

#[test]
fn schedule_active_run_seek_uses_existing_id_index_without_unrelated_run_growth() {
    let store = Store::open_memory("node").unwrap();
    let mut writer = store.connection.write();
    let transaction = writer.transaction().unwrap();
    insert_run(&transaction, "active", "running", "normal");
    insert_run(&transaction, "terminal", "failed", "terminal");
    for id in ["active", "terminal", "missing"] {
        insert_claim(
            &transaction,
            &format!("lookup-{id}"),
            &format!("schedule/{id}"),
            "schedule.work-started",
            json!({"fields":{"mission_run":format!("mission-run/{id}")}}),
        );
    }
    let plan = transaction
        .prepare(&format!(
            "EXPLAIN QUERY PLAN {}",
            Store::SCHEDULE_ACTIVE_STARTED_RUN_QUERY
        ))
        .unwrap()
        .query_map(["schedule/missing"], |row| row.get::<_, String>(3))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    println!("schedule active run plan: {plan:?}");
    assert!(
        plan.iter()
            .any(|row| row.contains("SEARCH run") && row.contains("(id=?)")),
        "{plan:?}"
    );
    assert!(!plan.iter().any(|row| row.contains("SCAN run")), "{plan:?}");
    let mut previous = None;
    let mut inserted = 0;
    for population in [100, 10_000] {
        for index in inserted..population {
            insert_run(
                &transaction,
                &format!("unrelated-{index}"),
                "running",
                "normal",
            );
        }
        inserted = population;
        let mut steps = 0;
        let mut statement = transaction
            .prepare(Store::SCHEDULE_ACTIVE_STARTED_RUN_QUERY)
            .unwrap();
        for (subject, expected) in [
            ("schedule/active", true),
            ("schedule/terminal", false),
            ("schedule/missing", false),
        ] {
            statement.reset_status(StatementStatus::VmStep);
            statement.reset_status(StatementStatus::FullscanStep);
            let answer: bool = statement.query_row([subject], |row| row.get(0)).unwrap();
            assert_eq!(answer, expected);
            let fullscan = statement.get_status(StatementStatus::FullscanStep);
            let vm = statement.get_status(StatementStatus::VmStep);
            assert_eq!(fullscan, 0, "{plan:?}");
            println!(
                "schedule active run population={population} subject={subject} answer={answer} fullscan={fullscan} vm={vm}"
            );
            steps += vm;
        }
        assert!(
            steps > 0 && steps <= 600,
            "population={population}, steps={steps}"
        );
        if let Some(prior) = previous {
            assert!(
                steps <= prior + 20,
                "unrelated runs must not add scan work: {prior} -> {steps}"
            );
        }
        previous = Some(steps);
    }
}

#[test]
fn schedule_active_run_seek_keeps_claim_and_run_at_one_reader_cut() {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(&directory.path().join("state.sqlite3"), "node").unwrap());
    {
        let writer = store.connection.write();
        insert_run(&writer, "cut-old", "failed", "terminal");
        insert_run(&writer, "cut-new", "running", "normal");
        insert_claim(
            &writer,
            "cut-started",
            "schedule/cut",
            "schedule.work-started",
            json!({"fields":{"mission_run":"mission-run/cut-old"}}),
        );
    }
    let assert_cut = |reference: &str, old: &str, new: &str| {
        let actual: (String, String, String) = store
            .readers
            .get()
            .query_row(
                "SELECT json_extract(body,'$.fields.mission_run'),
                (SELECT status FROM mission_runs WHERE id='cut-old'),
                (SELECT status FROM mission_runs WHERE id='cut-new')
             FROM claims WHERE id='cut-started'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            actual,
            (reference.to_owned(), old.to_owned(), new.to_owned())
        );
    };
    store
        .read_snapshot(|_| {
            assert_cut("mission-run/cut-old", "failed", "running");
            assert_result(&store, "schedule/cut", false);
            let updating = store.clone();
            // A writer on the pinned reader's own thread is prohibited. Use the normal
            // writer loan on another thread; do not bypass the pinned-read assertion.
            std::thread::spawn(move || {
                let mut writer = updating.connection.write();
                let transaction = writer.transaction().unwrap();
                // Both complete cuts are negative; either mixed claim/run cut is positive.
                transaction
                    .execute(
                        "UPDATE mission_runs SET status='running' WHERE id='cut-old'",
                        [],
                    )
                    .unwrap();
                transaction
                    .execute(
                        "UPDATE mission_runs SET status='failed' WHERE id='cut-new'",
                        [],
                    )
                    .unwrap();
                transaction
                    .execute(
                        "UPDATE claims SET body=?1 WHERE id='cut-started'",
                        [json!({"fields":{"mission_run":"mission-run/cut-new"}}).to_string()],
                    )
                    .unwrap();
                transaction.commit().unwrap();
            })
            .join()
            .unwrap();
            assert_cut("mission-run/cut-old", "failed", "running");
            assert_result(&store, "schedule/cut", false);
            Ok(())
        })
        .unwrap();
    assert_cut("mission-run/cut-new", "running", "failed");
    assert_result(&store, "schedule/cut", false);
    store
        .connection
        .write()
        .execute(
            "UPDATE mission_runs SET status='running' WHERE id='cut-new'",
            [],
        )
        .unwrap();
    assert_cut("mission-run/cut-new", "running", "running");
    assert_result(&store, "schedule/cut", true);
}

#[test]
fn schedule_active_run_seek_preserves_blob_storage_and_duplicate_typed_ids() {
    // Reader-only storage modeling: Store's digest JSON triggers reject blob IDs.
    // Minimal tables retain ID affinity/collation without those safeguards.
    for encoding in ["UTF-8", "UTF-16le", "UTF-16be"] {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(&format!(
                "PRAGMA encoding='{encoding}';
             CREATE TABLE claims(subject TEXT,kind TEXT,body TEXT);
             CREATE TABLE mission_runs(id TEXT PRIMARY KEY,status TEXT);",
            ))
            .unwrap();
        let actual: String = connection
            .query_row("PRAGMA encoding", [], |row| row.get(0))
            .unwrap();
        assert_eq!(actual, encoding);
        println!("schedule active run storage encoding={actual}");
        // Exercise ordinary text IDs as well as equal-byte text/blob identities.
        for id in ["text", "é/雪", "text-nul\0tail"] {
            connection
                .execute("INSERT INTO mission_runs VALUES(?1,'running')", [id])
                .unwrap();
        }
        for id in ["blob", "blob-nul\0tail"] {
            connection
                .execute(
                    "INSERT INTO mission_runs VALUES(CAST(?1 AS BLOB),'running')",
                    [id],
                )
                .unwrap();
        }
        connection
            .execute("INSERT INTO mission_runs VALUES('blob','failed')", [])
            .unwrap();
        for id in ["text", "é/雪", "text-nul\0tail", "blob", "blob-nul\0tail"] {
            connection
                .execute(
                    "INSERT INTO claims VALUES(?1,'schedule.work-started',?2)",
                    params![
                        format!("schedule/{id}"),
                        json!({"fields":{"mission_run":format!("mission-run/{id}")}}).to_string()
                    ],
                )
                .unwrap();
        }
        let check = |subject: &str, expected| {
            for query in [LEGACY_QUERY, Store::SCHEDULE_ACTIVE_STARTED_RUN_QUERY] {
                let answer: bool = connection
                    .query_row(query, [subject], |row| row.get(0))
                    .unwrap();
                assert_eq!(answer, expected, "{encoding}: {subject}");
            }
        };
        for id in ["text", "é/雪", "text-nul\0tail", "blob", "blob-nul\0tail"] {
            check(&format!("schedule/{id}"), true);
        }
        connection
            .execute(
                "UPDATE mission_runs SET status='completed' WHERE typeof(id)='blob'",
                [],
            )
            .unwrap();
        check("schedule/blob", false);
        check("schedule/blob-nul\0tail", false);
    }
}
