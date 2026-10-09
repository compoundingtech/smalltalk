//! Reader controls use synthetic retained rows, not a replication/admission fixture.
use super::*;
use rusqlite::StatementStatus;

#[test]
fn authored_pull_request_lookup_indexes_existing_schema_seventeen_on_open() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.sqlite3");
    let store = Store::open(&path, "node").unwrap();
    let connection = store.connection.write();
    connection
        .execute_batch(
            "DROP INDEX claims_authored_pull_request_url_index;
         DROP INDEX claims_authored_pull_request_number_index;",
        )
        .unwrap();
    insert(
        &connection,
        "existing",
        "resource/mission-run/author/pull-request",
        "resource.observed",
        json!({"url":"url", "repository":"repo", "number":42}),
        "1",
    );
    drop(connection);
    drop(store);
    let store = Store::open(&path, "node").unwrap();
    let connection = store.readers.get();
    assert_eq!(
        connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, u32>(0))
            .unwrap(),
        18
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name IN
         ('claims_authored_pull_request_url_index', 'claims_authored_pull_request_number_index')",
                [],
                |row| row.get::<_, u32>(0),
            )
            .unwrap(),
        2
    );
    assert_eq!(
        authoring_pull_request_runs_tx(&connection, &json!({"url":"url"})).unwrap(),
        ["mission-run/author"]
    );
}

fn insert(connection: &Connection, id: &str, subject: &str, kind: &str, facts: Value, time: &str) {
    connection
        .execute(
            "INSERT INTO batches(id,origin,replica_sequence,hash,accepted_at_unix_ms)
         VALUES(?1,'node',1,?1,?2)",
            params![id, time],
        )
        .unwrap();
    connection.execute(
        "INSERT INTO claims(id,batch_id,subject,kind,origin,body,predecessors,accepted_at_unix_ms)
         VALUES(?1,?1,?2,?3,'node',?4,'[]',?5)",
        params![id, subject, kind, json!({"fields":{"facts":facts}}).to_string(), time],
    ).unwrap();
}

#[test]
fn authored_pull_request_lookup_preserves_identity_head_and_canonical_order() {
    let store = Store::open_memory("node").unwrap();
    let mut writer = store.connection.write();
    let connection = writer.transaction().unwrap();
    // Insert in reverse canonical order, with URL-only, number-only and dual matches.
    for (id, run, facts, time) in [
        (
            "both",
            "dual",
            json!({"url":"url", "repository":"repo", "number":42, "head_sha":"head"}),
            "12",
        ),
        (
            "url",
            "url-only",
            json!({"url":"url", "repository":"other", "number":7, "head_sha":"head"}),
            "11",
        ),
        (
            "number",
            "number-only",
            json!({"repository":"repo", "number":42}),
            "9",
        ),
        (
            "old",
            "old-head",
            json!({"url":"url", "head_sha":"old"}),
            "8",
        ),
        ("duplicate-run", "dual", json!({"url":"url"}), "13"),
    ] {
        insert(
            &connection,
            id,
            &format!("resource/mission-run/{run}/pull-request"),
            "resource.observed",
            facts,
            time,
        );
    }
    let facts = json!({"url":"url", "repository":"repo", "number":42, "head_sha":"head"});
    insert(
        &connection,
        "wrong-subject",
        "resource/ordinary",
        "resource.observed",
        facts.clone(),
        "1",
    );
    insert(
        &connection,
        "wrong-kind",
        "resource/mission-run/ignored/pull-request",
        "custom.test",
        facts.clone(),
        "1",
    );
    let read = |facts: &Value| authoring_pull_request_runs_tx(&connection, facts).unwrap();
    assert_eq!(
        read(&facts),
        [
            "mission-run/number-only",
            "mission-run/url-only",
            "mission-run/dual"
        ]
    );
    assert_eq!(
        read(&json!({"url":"url", "head_sha":"head"})),
        ["mission-run/url-only", "mission-run/dual"]
    );
    assert_eq!(
        read(&json!({"repository":"repo", "number":42, "head_sha":"head"})),
        ["mission-run/number-only", "mission-run/dual"]
    );
    assert_eq!(
        read(&json!({"url":"url"})),
        [
            "mission-run/old-head",
            "mission-run/url-only",
            "mission-run/dual"
        ]
    );
    assert!(read(&json!({"head_sha":"head"})).is_empty());
    assert!(read(&json!({"repository":"repo"})).is_empty());
    assert!(read(&json!({"url":"absent", "repository":"repo", "number":43})).is_empty());

    // Ordinary SQLite index maintenance must reflect changed identities and removals.
    connection
        .execute(
            "UPDATE claims SET body=?1 WHERE id='number'",
            [json!({"fields":{"facts":{"repository":"other", "number":42}}}).to_string()],
        )
        .unwrap();
    connection
        .execute(
            "DELETE FROM claims WHERE id IN ('url', 'both', 'duplicate-run')",
            [],
        )
        .unwrap();
    assert!(read(&facts).is_empty());
}

#[test]
fn authored_pull_request_lookup_seeks_through_sparse_unrelated_history() {
    let store = Store::open_memory("node").unwrap();
    let mut connection = store.connection.write();
    let transaction = connection.transaction().unwrap();
    insert(
        &transaction,
        "match",
        "resource/mission-run/author/pull-request",
        "resource.observed",
        json!({"url":"url", "repository":"repo", "number":42, "head_sha":"head"}),
        "1",
    );
    let query = canonical_sql(AUTHORING_PULL_REQUEST_RUNS_QUERY);
    let plan = transaction
        .prepare(&format!("EXPLAIN QUERY PLAN {query}"))
        .unwrap()
        .query_map(params!["url", "repo", 42], |row| row.get::<_, String>(3))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
        .join("; ");
    for index in [
        "claims_authored_pull_request_url_index",
        "claims_authored_pull_request_number_index",
    ] {
        assert!(
            plan.contains(&format!("SEARCH claims USING INDEX {index}")),
            "{plan}"
        );
    }
    let mut costs = Vec::new();
    for n in 1..=10_000 {
        insert(
            &transaction,
            &format!("unrelated-{n}"),
            &format!("resource/mission-run/unrelated-{n}/pull-request"),
            "resource.observed",
            json!({"url":format!("other-{n}"), "repository":"other", "number":n}),
            "1",
        );
        // Non-authoring resource history, including facts that match, never enters the indexes.
        insert(
            &transaction,
            &format!("ordinary-{n}"),
            &format!("resource/ordinary/{n}"),
            "resource.observed",
            json!({"url":"url", "repository":"repo", "number":42}),
            "1",
        );
        if n == 100 || n == 10_000 {
            let mut total = 0;
            for (facts, expected) in [
                (
                    json!({"url":"url", "repository":"repo", "number":42}),
                    vec!["mission-run/author"],
                ),
                (
                    json!({"url":"absent", "repository":"repo", "number":43}),
                    vec![],
                ),
            ] {
                {
                    let statement = transaction.prepare_cached(&query).unwrap();
                    statement.reset_status(StatementStatus::VmStep);
                    statement.reset_status(StatementStatus::FullscanStep);
                }
                assert_eq!(
                    authoring_pull_request_runs_tx(&transaction, &facts).unwrap(),
                    expected
                );
                let statement = transaction.prepare_cached(&query).unwrap();
                assert_eq!(
                    statement.get_status(StatementStatus::FullscanStep),
                    0,
                    "authored PR identity lookup must seek: {plan}"
                );
                total += statement.get_status(StatementStatus::VmStep);
            }
            costs.push(total);
        }
    }
    assert!(
        costs[0] > 0 && costs[1] <= costs[0] * 2 && costs[1] < 500,
        "positive and absent ownership lookups must stay bounded as unrelated history grows: {costs:?}"
    );
}
