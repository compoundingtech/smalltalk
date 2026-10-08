//! Incoming declaration edges, derived transactionally from the selected declarations.
use super::*;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS declared_resource_edges (
    owner TEXT NOT NULL,
    relation TEXT NOT NULL DEFAULT 'declared-resource',
    name TEXT NOT NULL,
    target TEXT NOT NULL,
    reason TEXT,
    PRIMARY KEY(owner, relation, name)
);
CREATE INDEX IF NOT EXISTS declared_resource_edges_target ON declared_resource_edges(target, owner, relation, name);
CREATE TRIGGER IF NOT EXISTS declared_resource_agent_insert AFTER INSERT ON desired BEGIN
    INSERT INTO declared_resource_edges(owner, name, target, reason)
    SELECT NEW.subject, json_extract(value,'$.name'), json_extract(value,'$.subject'), json_extract(value,'$.reason')
    FROM json_each(NEW.body,'$.resources') WHERE NEW.kind='agent';
END;
CREATE TRIGGER IF NOT EXISTS declared_resource_agent_update AFTER UPDATE ON desired BEGIN
    DELETE FROM declared_resource_edges WHERE owner=OLD.subject AND relation='declared-resource';
    INSERT INTO declared_resource_edges(owner, name, target, reason)
    SELECT NEW.subject, json_extract(value,'$.name'), json_extract(value,'$.subject'), json_extract(value,'$.reason')
    FROM json_each(NEW.body,'$.resources') WHERE NEW.kind='agent';
END;
CREATE TRIGGER IF NOT EXISTS declared_resource_agent_delete AFTER DELETE ON desired BEGIN
    DELETE FROM declared_resource_edges WHERE owner=OLD.subject AND relation='declared-resource';
END;
CREATE TRIGGER IF NOT EXISTS declared_resource_mission_insert AFTER INSERT ON mission_definitions BEGIN
    INSERT INTO declared_resource_edges(owner, name, target, reason)
    SELECT 'mission/' || NEW.mission_id, json_extract(edge.value,'$.name'), json_extract(edge.value,'$.subject'), json_extract(edge.value,'$.reason')
    FROM mission_revisions r, json_each(r.body,'$.resources') edge
    WHERE r.mission_id=NEW.mission_id AND r.revision=NEW.revision;
END;
CREATE TRIGGER IF NOT EXISTS declared_resource_mission_update AFTER UPDATE ON mission_definitions BEGIN
    DELETE FROM declared_resource_edges WHERE owner='mission/' || OLD.mission_id AND relation='declared-resource';
    INSERT INTO declared_resource_edges(owner, name, target, reason)
    SELECT 'mission/' || NEW.mission_id, json_extract(edge.value,'$.name'), json_extract(edge.value,'$.subject'), json_extract(edge.value,'$.reason')
    FROM mission_revisions r, json_each(r.body,'$.resources') edge
    WHERE r.mission_id=NEW.mission_id AND r.revision=NEW.revision;
END;
CREATE TRIGGER IF NOT EXISTS declared_resource_mission_delete AFTER DELETE ON mission_definitions BEGIN
    DELETE FROM declared_resource_edges WHERE owner='mission/' || OLD.mission_id AND relation='declared-resource';
END;
"#;

/// meta key set once every preexisting declaration is folded into the edge table.
const VERSION_KEY: &str = "declared_resource_edges_version";
/// meta key holding the backfill's resume point: `agents:<subject>` or `missions:<mission id>`.
const CURSOR_KEY: &str = "declared_resource_edges_cursor";

/// Declaration revisions one backfill step folds before it gives the writer back, so folding
/// a preexisting store is bounded by construction rather than by the store's size.
pub(super) const BACKFILL_BATCH: usize = 64;

/// What one backfill step did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct BackfillStep {
    /// Every preexisting declaration is folded in, and the edge table is the read path again.
    pub complete: bool,
    /// Declaration revisions this step folded.
    pub folded: usize,
}

pub(super) fn create_schema(connection: &Connection) -> Result<()> {
    // Upgrade the same reverse index, preserving its existing selected-declaration rows.
    // The discriminator prevents authored resource names from colliding with pair records.
    connection.execute_batch("SAVEPOINT ordered_membership_reverse_index_upgrade")?;
    let result = (|| -> Result<()> {
    let exists: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='declared_resource_edges')",
        [], |row| row.get(0),
    )?;
    let has_relation = exists && connection.prepare("PRAGMA table_info(declared_resource_edges)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?.iter().any(|column| column == "relation");
    if exists && !has_relation {
    for trigger in ["agent_insert", "agent_update", "agent_delete", "mission_insert", "mission_update", "mission_delete"] {
        connection.execute_batch(&format!("DROP TRIGGER IF EXISTS declared_resource_{trigger}"))?;
    }
        connection.execute_batch(
            "DROP INDEX IF EXISTS declared_resource_edges_target;
             ALTER TABLE declared_resource_edges RENAME TO declared_resource_edges_v1;",
        )?;
    }
    connection.execute_batch(SCHEMA)?;
    if exists && !has_relation {
        connection.execute_batch(
            "INSERT INTO declared_resource_edges(owner,name,target,reason)
             SELECT owner,name,target,reason FROM declared_resource_edges_v1;
             DROP TABLE declared_resource_edges_v1;",
        )?;
    }
        Ok(())
    })();
    connection.execute_batch(if result.is_ok() {
        "RELEASE ordered_membership_reverse_index_upgrade"
    } else {
        "ROLLBACK TO ordered_membership_reverse_index_upgrade; RELEASE ordered_membership_reverse_index_upgrade"
    })?;
    result
}

/// Bring the edge projection up to date as the store opens. A shared-memory store holds a
/// test's or a tool's handful of declarations, so it converges here, inside the open
/// transaction. A durable store never waits at open: this only seeds the resume cursor, the
/// thread [`spawn_backfill`] starts finishes the work in [`BACKFILL_BATCH`]-sized writer
/// transactions, and reads answer from the declarations
/// ([`referrers_from_declarations`]) until it does.
pub(super) fn open(transaction: &Transaction<'_>, shared_memory: bool) -> Result<()> {
    if complete_tx(transaction)? {
        return Ok(());
    }
    if shared_memory {
        while !backfill_step_tx(transaction, BACKFILL_BATCH)?.complete {}
        return Ok(());
    }
    seed_cursor_tx(transaction)?;
    Ok(())
}

fn seed_cursor_tx(transaction: &Transaction<'_>) -> Result<()> {
    transaction.execute(
        &format!("INSERT OR IGNORE INTO meta(key,value) VALUES('{CURSOR_KEY}','agents:')"),
        [],
    )?;
    Ok(())
}

fn complete_tx(connection: &Connection) -> Result<bool> {
    Ok(connection.query_row(
        &format!("SELECT EXISTS(SELECT 1 FROM meta WHERE key='{VERSION_KEY}' AND value='1')"),
        [],
        |row| row.get(0),
    )?)
}

/// Whether the edge table is still being folded from preexisting declarations, so a read must
/// answer from the declarations themselves rather than from the partial index.
pub(super) fn backfilling(connection: &Connection) -> Result<bool> {
    Ok(!complete_tx(connection)?)
}

/// One bounded backfill step: fold the next `limit` selected declaration revisions into the
/// edge table — selected agents first, then selected missions, each ordered by owner — and
/// advance the durable cursor. Re-running a step is idempotent: an owner's edges are replaced
/// wholesale, exactly as the declaration triggers replace them.
pub(super) fn backfill_step_tx(transaction: &Transaction<'_>, limit: usize) -> Result<BackfillStep> {
    let cursor: Option<String> = transaction
        .query_row(
            &format!("SELECT value FROM meta WHERE key='{CURSOR_KEY}'"),
            [],
            |row| row.get(0),
        )
        .optional()?;
    let Some(cursor) = cursor else {
        if complete_tx(transaction)? {
            return Ok(BackfillStep {
                complete: true,
                folded: 0,
            });
        }
        seed_cursor_tx(transaction)?;
        return Ok(BackfillStep {
            complete: false,
            folded: 0,
        });
    };
    let (phase, after) = cursor.split_once(':').unwrap_or(("agents", ""));
    let phase = if phase == "missions" { "missions" } else { "agents" };
    let revisions: Vec<(String, String, String)> = if phase == "missions" {
        let mut statement = transaction.prepare(
            "SELECT 'mission/' || d.mission_id, d.mission_id, r.body
             FROM mission_definitions d
             JOIN mission_revisions r ON r.mission_id=d.mission_id AND r.revision=d.revision
             WHERE d.mission_id > ?1 ORDER BY d.mission_id LIMIT ?2",
        )?;
        let rows = statement.query_map(params![after, limit as i64], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    } else {
        let mut statement = transaction.prepare(
            "SELECT subject, subject, body FROM desired
             WHERE kind='agent' AND subject > ?1 ORDER BY subject LIMIT ?2",
        )?;
        let rows = statement.query_map(params![after, limit as i64], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let folded = revisions.len();
    for (owner, _, body) in &revisions {
        transaction.execute(
            "DELETE FROM declared_resource_edges WHERE owner=?1 AND relation='declared-resource'",
            [owner],
        )?;
        transaction.execute(
            "INSERT INTO declared_resource_edges(owner,name,target,reason)
             SELECT ?1, json_extract(value,'$.name'), json_extract(value,'$.subject'), json_extract(value,'$.reason')
             FROM json_each(?2,'$.resources')",
            params![owner, body],
        )?;
    }
    let Some((_, key, _)) = revisions.last() else {
        // A phase ran out of declarations: the agents give way to the missions, and a
        // missions phase that runs out completes the backfill.
        if phase == "agents" {
            transaction.execute(
                &format!("UPDATE meta SET value='missions:' WHERE key='{CURSOR_KEY}'"),
                [],
            )?;
            return Ok(BackfillStep {
                complete: false,
                folded,
            });
        }
        transaction.execute(
            &format!("DELETE FROM meta WHERE key='{CURSOR_KEY}'"),
            [],
        )?;
        transaction.execute(
            &format!("INSERT OR REPLACE INTO meta(key,value) VALUES('{VERSION_KEY}','1')"),
            [],
        )?;
        return Ok(BackfillStep {
            complete: true,
            folded,
        });
    };
    transaction.execute(
        &format!("UPDATE meta SET value=?1 WHERE key='{CURSOR_KEY}'"),
        params![format!("{phase}:{key}")],
    )?;
    Ok(BackfillStep {
        complete: false,
        folded,
    })
}

/// Run one bounded backfill step as a batched write on the store's writer, sharing its
/// transaction discipline with every other write; the writer is free again between steps.
#[cfg(not(test))]
pub(crate) fn backfill_step(
    jobs: &std::sync::mpsc::Sender<WriterJob>,
) -> Result<BackfillStep, String> {
    let outcome = Arc::new(Mutex::new(None));
    let slot = outcome.clone();
    let run: Box<dyn FnOnce(&Transaction<'_>) -> bool + Send> =
        Box::new(move |transaction| {
            let result = backfill_step_tx(transaction, BACKFILL_BATCH);
            let succeeded = result.is_ok();
            *slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(result);
            succeeded
        });
    let (done, done_here) = std::sync::mpsc::sync_channel(1);
    jobs.send(WriterJob::Batched {
        run,
        profile: None,
        wait: None,
        done,
    })
    .map_err(|_| "the writer queue is closed".to_owned())?;
    let committed = done_here
        .recv()
        .map_err(|_| "the writer thread stopped".to_owned())?;
    committed?;
    let result = outcome
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take();
    result
        .unwrap_or_else(|| Err(anyhow::Error::msg("the backfill step did not run")))
        .map_err(|error| error.to_string())
}

/// Finish the edge backfill off the open path: one bounded step per writer transaction,
/// yielding between steps, until the projection is complete. `Store::open` starts this once
/// the store is otherwise ready, so open itself never waits on declaration history.
#[cfg(not(test))]
pub(super) fn spawn_backfill(connection: &WriterConnection) {
    let Some(jobs) = connection.jobs.lock().ok().and_then(|jobs| jobs.clone()) else {
        return;
    };
    let _ = std::thread::Builder::new()
        .name("st3-resource-backfill".into())
        .spawn(move || loop {
            match backfill_step(&jobs) {
                Ok(step) if !step.complete => {}
                Ok(_) => return,
                Err(error) => {
                    tracing::warn!(%error, "declared resource backfill failed; declaration reads remain active");
                    return;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        });
}

/// The same referrers read straight from the selected declarations, for while a preexisting
/// store's edge index is still being folded. Same rows and order as the indexed read.
pub(super) fn referrers_from_declarations(
    connection: &Connection,
    subject: &str,
) -> Result<Vec<Value>> {
    let mut statement = connection.prepare(
        "SELECT owner, name, reason FROM (
            SELECT d.subject AS owner,
                   json_extract(e.value,'$.name') AS name,
                   json_extract(e.value,'$.reason') AS reason
            FROM desired d, json_each(d.body,'$.resources') e
            WHERE d.kind='agent' AND json_extract(e.value,'$.subject')=?1
            UNION ALL
            SELECT 'mission/' || d.mission_id AS owner,
                   json_extract(e.value,'$.name') AS name,
                   json_extract(e.value,'$.reason') AS reason
            FROM mission_definitions d
            JOIN mission_revisions r ON r.mission_id=d.mission_id AND r.revision=d.revision,
                 json_each(r.body,'$.resources') e
            WHERE json_extract(e.value,'$.subject')=?1
        ) ORDER BY owner, name",
    )?;
    let rows = statement.query_map([subject], |row| {
        Ok(json!({
            "owner": row.get::<_, String>(0)?,
            "name": row.get::<_, String>(1)?,
            "reason": row.get::<_, Option<String>>(2)?,
        }))
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn existing_reverse_index_upgrades_atomically_without_relation_collisions() {
        let connection = Connection::open_in_memory().unwrap();
        connection.execute_batch(
            "CREATE TABLE desired(subject TEXT PRIMARY KEY,kind TEXT,body TEXT);
             CREATE TABLE mission_definitions(mission_id TEXT PRIMARY KEY,revision TEXT);
             CREATE TABLE mission_revisions(mission_id TEXT,revision TEXT,body TEXT);
             CREATE TABLE declared_resource_edges(owner TEXT,name TEXT,target TEXT,reason TEXT,PRIMARY KEY(owner,name));
             CREATE INDEX declared_resource_edges_target ON declared_resource_edges(target,owner,name);
             INSERT INTO desired VALUES('agent/seat','agent','{\"resources\":[{\"name\":\"goal\",\"subject\":\"resource/goal\",\"reason\":\"original\"}]}');
             INSERT INTO declared_resource_edges VALUES('agent/seat','goal','resource/goal','original');",
        ).unwrap();
        create_schema(&connection).unwrap();
        let row: (String,String,String) = connection.query_row(
            "SELECT relation,target,reason FROM declared_resource_edges", [],
            |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)),
        ).unwrap();
        assert_eq!(row, ("declared-resource".into(),"resource/goal".into(),"original".into()));
        connection.execute(
            "INSERT INTO declared_resource_edges(owner,relation,name,target) VALUES('agent/seat','ordered-membership','goal','resource/member')", [],
        ).unwrap();
        connection.execute("UPDATE desired SET body='{\"resources\":[]}' WHERE subject='agent/seat'", []).unwrap();
        let relations: Vec<String> = connection.prepare("SELECT relation FROM declared_resource_edges").unwrap()
            .query_map([], |row| row.get(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap();
        assert_eq!(relations, ["ordered-membership"]);
        create_schema(&connection).unwrap();
        let count: usize = connection.query_row("SELECT COUNT(*) FROM declared_resource_edges", [], |row| row.get(0)).unwrap();
        assert_eq!(count, 1, "reopening the upgraded schema preserves the same reverse index");
    }

    #[test]
    fn incoming_resource_edges_follow_replacement_replication_and_replay() {
        let store = Store::open_memory("node").unwrap();
        let publish = |resources: &str, key: &str| {
            let kdl = format!(
                "version 2\nagent \"ada/seat\" {{ workspace \"/tmp\"; command \"true\"; {resources} }}\n\
                 mission \"ada/work\" state=\"ready\" {{ goal \"Work.\"; {resources} step \"work\" {{ }} }}\n"
            );
            let intent = crate::graph::parse_test_intent(&kdl, "node").unwrap();
            let preview = store.mission(&intent, IntentInput {
                kdl, source_name: None,
            }).unwrap();
            store.apply(&intent, &preview.subject_tokens, key).unwrap();
        };
        publish("resource \"goal\" uri=\"https://example.com/goal\" reason=\"shared goal\";", "first");
        let subject = format!("resource/uri/{}", hex::encode(Sha256::digest(b"https://example.com/goal")));
        let expected = json!([
            {"owner":"agent/ada/seat","name":"goal","reason":"shared goal"},
            {"owner":"mission/ada/work","name":"goal","reason":"shared goal"}
        ]);
        assert_eq!(json!(store.declared_resource_referrers(&subject).unwrap()), expected);
        {
            let mut connection = store.connection.write();
            let transaction = connection.transaction().unwrap();
            // Simulate opening a store written before this projection existed.
            transaction.execute("DELETE FROM declared_resource_edges", []).unwrap();
            transaction.execute("DELETE FROM meta WHERE key='declared_resource_edges_version'", []).unwrap();
            open(&transaction, true).unwrap();
            transaction.commit().unwrap();
        }
        assert_eq!(json!(store.declared_resource_referrers(&subject).unwrap()), expected);
        let replica = Store::open_memory("peer").unwrap();
        replica.import_replication("node", &store.export_replication(0).unwrap()).unwrap();
        assert_eq!(json!(replica.declared_resource_referrers(&subject).unwrap()), expected);
        {
            let mut connection = replica.connection.write();
            let transaction = connection.transaction().unwrap();
            replay_graph_from_nothing_tx(&transaction).unwrap();
            transaction.commit().unwrap();
        }
        assert_eq!(json!(replica.declared_resource_referrers(&subject).unwrap()), expected);
        publish("", "removed");
        assert_eq!(store.declared_resource_referrers(&subject).unwrap(), Vec::<Value>::new());
        replica.import_replication("node", &store.export_replication(0).unwrap()).unwrap();
        assert_eq!(replica.declared_resource_referrers(&subject).unwrap(), Vec::<Value>::new());
    }

    /// `count` agents and `count` missions, every one referencing `resource/shared`.
    fn publish_many(store: &Store, count: usize) {
        for i in 0..count {
            let kdl = format!(
                "version 2\nagent \"ada/seat-{i}\" {{ workspace \"/tmp\"; command \"true\"; \
                 resource \"goal\" subject=\"resource/shared\"; }}\n\
                 mission \"ada/work-{i}\" state=\"ready\" {{ goal \"Work.\"; \
                 resource \"goal\" subject=\"resource/shared\"; step \"work\" {{ }} }}\n"
            );
            let intent = crate::graph::parse_test_intent(&kdl, "node").unwrap();
            let preview = store
                .mission(&intent, IntentInput {
                    kdl,
                    source_name: None,
                })
                .unwrap();
            store
                .apply(&intent, &preview.subject_tokens, &format!("publish-{i}"))
                .unwrap();
        }
    }

    /// Simulate a store written before this projection existed: edges gone, no version and no
    /// cursor, exactly what a durable open of an upgraded store finds.
    fn wipe_projection(store: &Store) {
        let mut connection = store.connection.write();
        let transaction = connection.transaction().unwrap();
        transaction
            .execute("DELETE FROM declared_resource_edges", [])
            .unwrap();
        transaction
            .execute(
                "DELETE FROM meta WHERE key IN ('declared_resource_edges_version','declared_resource_edges_cursor')",
                [],
            )
            .unwrap();
        transaction.commit().unwrap();
    }

    #[test]
    fn backfill_of_a_preexisting_store_runs_in_bounded_resumable_batches() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(&directory.path().join("store.sqlite3"), "node").unwrap();
        publish_many(&store, 40);
        wipe_projection(&store);
        // Reads stay complete while the index is empty: they answer from the declarations.
        assert_eq!(
            store
                .declared_resource_referrers("resource/shared")
                .unwrap()
                .len(),
            80
        );
        let mut steps = 0;
        let mut folded_per_step = Vec::new();
        loop {
            let mut connection = store.connection.write();
            let transaction = connection.transaction().unwrap();
            let step = backfill_step_tx(&transaction, 16).unwrap();
            transaction.commit().unwrap();
            folded_per_step.push(step.folded);
            steps += 1;
            assert!(steps <= 20, "the backfill must converge: {folded_per_step:?}");
            if step.complete {
                break;
            }
        }
        assert!(
            steps >= 80 / 16,
            "more declarations than one batch means several batches: {steps} steps"
        );
        assert!(
            folded_per_step.iter().all(|&folded| folded <= 16),
            "{folded_per_step:?}"
        );
        assert_eq!(folded_per_step.iter().sum::<usize>(), 80);
        assert_eq!(
            store
                .declared_resource_referrers("resource/shared")
                .unwrap()
                .len(),
            80
        );
        let connection = store.readers.get();
        assert!(!backfilling(&connection).unwrap());
        let cursor: Option<String> = connection
            .query_row(
                "SELECT value FROM meta WHERE key='declared_resource_edges_cursor'",
                [],
                |row| row.get(0),
            )
            .optional()
            .unwrap();
        assert!(cursor.is_none(), "a complete backfill leaves no cursor");
    }

    #[test]
    fn backfill_resumes_from_its_cursor_after_a_crash_mid_migration() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("store.sqlite3");
        {
            let store = Store::open(&path, "node").unwrap();
            publish_many(&store, 30);
            wipe_projection(&store);
            // A durable open seeds the cursor without folding anything; two batches run,
            // then the store "crashes" mid-migration.
            let mut folded = 0;
            for _ in 0..2 {
                let mut connection = store.connection.write();
                let transaction = connection.transaction().unwrap();
                open(&transaction, false).unwrap();
                let step = backfill_step_tx(&transaction, 16).unwrap();
                transaction.commit().unwrap();
                folded += step.folded;
                assert!(!step.complete);
            }
            assert_eq!(folded, 30);
        }
        {
            let store = Store::open(&path, "node").unwrap();
            let connection = store.readers.get();
            let cursor: String = connection
                .query_row(
                    "SELECT value FROM meta WHERE key='declared_resource_edges_cursor'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(
                cursor.starts_with("agents:agent/ada/seat-"),
                "the cursor survived the crash: {cursor}"
            );
            assert!(backfilling(&connection).unwrap());
        }
        let store = Store::open(&path, "node").unwrap();
        loop {
            let mut connection = store.connection.write();
            let transaction = connection.transaction().unwrap();
            let step = backfill_step_tx(&transaction, BACKFILL_BATCH).unwrap();
            transaction.commit().unwrap();
            if step.complete {
                break;
            }
        }
        let connection = store.readers.get();
        let expected = referrers_from_declarations(&connection, "resource/shared").unwrap();
        assert_eq!(expected.len(), 60);
        drop(connection);
        // What the resumed backfill built is what the declarations say, and the indexed read
        // is the read path again.
        assert_eq!(
            store.declared_resource_referrers("resource/shared").unwrap(),
            expected
        );
    }

    /// Prints the open-time and per-batch cost of converging a copied store. Runs only when
    /// `PR1144_STORE` names one; ordinary test runs skip it.
    #[test]
    fn pr1144_profile_backfill_on_a_copied_store() {
        let Ok(path) = std::env::var("PR1144_STORE") else {
            return;
        };
        {
            let store = Store::open(std::path::Path::new(&path), "pr1144-measure").unwrap();
            wipe_projection(&store);
        }
        let opened = std::time::Instant::now();
        let store = Store::open(std::path::Path::new(&path), "pr1144-measure").unwrap();
        println!(
            "{{\"pr1144_open_ms\":{:.3}}}",
            opened.elapsed().as_secs_f64() * 1000.0
        );
        loop {
            let mut connection = store.connection.write();
            let transaction = connection.transaction().unwrap();
            let started = std::time::Instant::now();
            let step = backfill_step_tx(&transaction, BACKFILL_BATCH).unwrap();
            transaction.commit().unwrap();
            drop(connection);
            println!(
                "{{\"pr1144_backfill_step_ms\":{:.3},\"folded\":{}}}",
                started.elapsed().as_secs_f64() * 1000.0,
                step.folded
            );
            // The declaration fallback answers while the backfill runs.
            assert_eq!(
                store
                    .declared_resource_referrers("resource/pr1144-nonexistent")
                    .unwrap(),
                Vec::<Value>::new()
            );
            if step.complete {
                break;
            }
        }
        let connection = store.readers.get();
        assert!(!backfilling(&connection).unwrap());
    }
}
