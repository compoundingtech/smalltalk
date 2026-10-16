use super::*;

// Independent immediate-parent point query, with no batching/helper delegation.
fn point(connection: &Connection, subject: &str) -> Result<Version> {
    Ok(connection.query_row(
        "SELECT (SELECT COALESCE(MAX(store_index), 0) FROM claims WHERE subject=?1),
                (SELECT claim_id FROM desired WHERE subject=?1)",
        [subject],
        |row| {
            Ok(Version {
                latest_claim_index: row.get(0)?,
                desired_claim_id: row.get(1)?,
            })
        },
    )?)
}

fn fixture() -> Connection {
    let mut connection = Connection::open_in_memory().unwrap();
    smallclaims::sqlite::observe(&mut connection);
    connection
        .execute_batch(
            "CREATE TABLE claims(store_index INTEGER PRIMARY KEY,subject TEXT,kind TEXT);
         CREATE INDEX claims_subject_kind_index ON claims(subject,kind,store_index);
         CREATE TABLE desired(subject TEXT PRIMARY KEY,claim_id TEXT NOT NULL);
         INSERT INTO claims VALUES(4,'message/a','message.sent'),(9,'message/a','message.closed'),
             (7,'message/b','message.sent');
         INSERT INTO desired VALUES('message/a','desired/first');",
        )
        .unwrap();
    connection
}

#[test]
fn batch_versions_match_point_reads_with_missing_duplicate_and_replaced_inputs() {
    let connection = fixture();
    let subjects = ["message/b", "message/missing", "message/a", "message/a"];
    for change in [
        "SELECT 1",
        "UPDATE desired SET claim_id='desired/replaced' WHERE subject='message/a'",
        "DELETE FROM desired WHERE subject='message/a'",
        "DELETE FROM claims WHERE store_index=9",
    ] {
        connection.execute_batch(change).unwrap();
        let expected = subjects
            .iter()
            .map(|subject| point(&connection, subject).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(read(&connection, &subjects).unwrap(), expected);
    }
    connection
        .execute_batch("INSERT INTO desired VALUES('message/a',X'ff')")
        .unwrap();
    assert!(
        point(&connection, "message/a").is_err(),
        "point reader must reject malformed key types"
    );
    assert!(
        read(&connection, &subjects).is_err(),
        "batch must preserve errors, not manufacture an absent key"
    );
}

#[test]
fn version_batch_is_one_statement_with_an_explicit_128_subject_frontier() {
    let connection = fixture();
    // STATEMENTS_RUN is incremented by sqlite::record_sqlite_time through the
    // completion/profile callback installed by observe(), not by prepare_cached.
    for count in [1, 2, 127, 128] {
        let subjects = vec!["message/a"; count];
        let before = STATEMENTS_RUN.with(std::cell::Cell::get);
        let batched = read(&connection, &subjects).unwrap();
        assert_eq!(STATEMENTS_RUN.with(std::cell::Cell::get) - before, 1);
        let before = STATEMENTS_RUN.with(std::cell::Cell::get);
        let expected = subjects
            .iter()
            .map(|subject| point(&connection, subject).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(STATEMENTS_RUN.with(std::cell::Cell::get) - before, count);
        assert_eq!(batched, expected);
    }
    let before = STATEMENTS_RUN.with(std::cell::Cell::get);
    assert!(read(&connection, &[]).unwrap().is_empty());
    assert!(read(&connection, &["message/a"; 129]).is_err());
    assert_eq!(
        STATEMENTS_RUN.with(std::cell::Cell::get),
        before,
        "empty/rejected input executes no query"
    );
}

#[test]
fn batch_metadata_uses_only_supplied_subject_seeks() {
    let connection = fixture();
    connection
        .execute_batch(
            "WITH RECURSIVE n(x) AS (SELECT 10 UNION ALL SELECT x+1 FROM n WHERE x<10000)
        INSERT INTO claims SELECT x,'unrelated/'||x,'message.sent' FROM n;",
        )
        .unwrap();
    let sql = format!("EXPLAIN QUERY PLAN {QUERY}");
    let plan = connection
        .prepare(&sql)
        .unwrap()
        .query_map(["[\"message/a\",\"message/missing\"]"], |row| {
            row.get::<_, String>(3)
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert!(
        plan.iter().any(
            |step| step.contains("SEARCH claims") && step.contains("claims_subject_kind_index")
        ),
        "{plan:?}"
    );
    assert!(
        plan.iter()
            .any(|step| step.contains("SEARCH desired")
                && step.contains("sqlite_autoindex_desired_1")),
        "{plan:?}"
    );
    assert!(
        plan.iter()
            .any(|step| step.contains("SCAN requested VIRTUAL TABLE"))
            && plan
                .iter()
                .filter(|step| step.contains("SCAN "))
                .all(|step| step.contains("SCAN requested VIRTUAL TABLE")),
        "{plan:?}"
    );
    assert_eq!(
        read(&connection, &["message/a"]).unwrap(),
        vec![point(&connection, "message/a").unwrap()]
    );
}

fn send(store: &Store, id: &str, to: &str, tags: Vec<String>) {
    store
        .append_claim(&ClaimInput {
            subject: format!("message/{id}"),
            kind: "message.sent".into(),
            actor: Some("daemon/runtime".into()),
            fields: BTreeMap::from([
                ("from".into(), json!("daemon/runtime")),
                ("to".into(), json!(to)),
                ("content".into(), json!("invented wake body")),
                ("status".into(), json!("sent")),
                ("tags".into(), json!(tags)),
            ]),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
}

// Verbatim parent selection/order/filtering, retaining its point cache validation.
fn point_work_wakes(store: &Store, agent: &str) -> Result<Vec<MessageView>> {
    let recipient = normalize_message_party(agent);
    let bare_recipient = recipient
        .strip_prefix("agent/")
        .filter(|suffix| !suffix.contains('/'))
        .unwrap_or(&recipient);
    let connection = store.readers.get();
    let mut statement = connection.prepare_cached(
        "SELECT DISTINCT sent.subject,
                (SELECT MIN(created.store_index) FROM claims created
                 WHERE created.subject=sent.subject) AS created_index
         FROM claims sent INDEXED BY claims_message_to_index
         WHERE sent.kind='message.sent'
           AND json_extract(sent.body, '$.fields.to') IN (?1, ?2)
           AND instr(sent.body, 'st3-work:')>0
         ORDER BY created_index, sent.subject",
    )?;
    let subjects = statement
        .query_map(params![recipient, bare_recipient], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(statement);
    let mut messages = Vec::with_capacity(subjects.len());
    for (subject, created_index) in subjects {
        let message = store.message_view_cached(&connection, &subject, created_index)?;
        if message.to == recipient && message.tags.iter().any(|tag| tag.starts_with("st3-work:")) {
            messages.push(message);
        }
    }
    sort_messages_canonically(&connection, &mut messages)?;
    Ok(messages)
}

#[test]
fn work_wake_batch_preserves_closed_attempts_order_and_fresh_lifecycle_without_point_queries() {
    let store = Store::open_memory("fixture").unwrap();
    for index in 0..129 {
        send(
            &store,
            &format!("wake-{index:03}"),
            "agent/worker",
            vec![format!("st3-work:step-run/{index}@1@1@worker")],
        );
    }
    send(&store, "ordinary", "agent/worker", vec![]);
    send(
        &store,
        "other",
        "agent/other",
        vec!["st3-work:step-run/other@1@1@other".into()],
    );
    // Warm through the original point path, so both calls see the same warm cache.
    let expected = point_work_wakes(&store, "agent/worker").unwrap();
    assert_eq!(expected.len(), 129);
    let before = STATEMENTS_RUN.with(std::cell::Cell::get);
    let point_result = point_work_wakes(&store, "agent/worker").unwrap();
    let point_count = STATEMENTS_RUN.with(std::cell::Cell::get) - before;
    let before = STATEMENTS_RUN.with(std::cell::Cell::get);
    let batch_result = store
        .work_wake_messages_for_reconcile("agent/worker")
        .unwrap();
    let batch_count = STATEMENTS_RUN.with(std::cell::Cell::get) - before;
    assert_eq!(
        serde_json::to_value(&batch_result).unwrap(),
        serde_json::to_value(&point_result).unwrap()
    );
    assert!(
        point_count >= batch_count + 127,
        "point={point_count}, batch={batch_count}"
    );
    store
        .append_claim(&ClaimInput {
            subject: "message/wake-000".into(),
            kind: "message.closed".into(),
            actor: Some("daemon/runtime".into()),
            fields: BTreeMap::from([("status".into(), json!("closed"))]),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    let actual = store
        .work_wake_messages_for_reconcile("agent/worker")
        .unwrap();
    assert_eq!(
        actual.len(),
        129,
        "closed attempts must remain for retry accounting"
    );
    assert_eq!(
        actual
            .iter()
            .find(|m| m.subject == "message/wake-000")
            .unwrap()
            .status,
        "closed"
    );
    assert_eq!(
        serde_json::to_value(&actual).unwrap(),
        serde_json::to_value(point_work_wakes(&store, "agent/worker").unwrap()).unwrap()
    );
}
