//! Local, rebuildable launch evidence. Source triggers record exact deltas; a native
//! transaction finalizer derives canonical keys after all admission metadata is present.
use super::*;

pub(super) const PAGE: usize = 64;
const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS local_publication_claims (
    claim_id TEXT PRIMARY KEY,
    subject TEXT NOT NULL,
    origin TEXT NOT NULL,
    kind TEXT NOT NULL,
    incarnation TEXT NOT NULL,
    sort_key BLOB NOT NULL
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS local_publication_claims_lookup
ON local_publication_claims(subject,kind,origin,incarnation,sort_key);
CREATE INDEX IF NOT EXISTS local_publication_runtime_lookup
ON local_publication_claims(subject,kind,origin,sort_key);
CREATE TABLE IF NOT EXISTS local_publication_pending (claim_id TEXT PRIMARY KEY) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS local_publication_batches (batch_id TEXT PRIMARY KEY, cursor INTEGER NOT NULL DEFAULT 0) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS local_publication_starts (
    observation_id INTEGER PRIMARY KEY,
    subject TEXT NOT NULL,
    incarnation TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS local_publication_starts_lookup
ON local_publication_starts(subject,incarnation,observation_id);
CREATE TABLE IF NOT EXISTS local_publication_bootstrap (
    singleton INTEGER PRIMARY KEY CHECK(singleton=1),
    phase INTEGER NOT NULL,
    cursor INTEGER NOT NULL,
    ceiling INTEGER NOT NULL,
    local_subject TEXT NOT NULL DEFAULT '',
    local_kind TEXT NOT NULL DEFAULT '',
    local_id INTEGER NOT NULL DEFAULT 0,
    local_ceiling INTEGER NOT NULL,
    progress_ms TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS local_publication_launch_epoch (
    singleton INTEGER PRIMARY KEY CHECK(singleton=1),
    generation INTEGER NOT NULL
);
INSERT OR IGNORE INTO local_publication_launch_epoch VALUES(1,0);
CREATE TRIGGER IF NOT EXISTS publication_launch_insert AFTER INSERT ON local_publication_claims
WHEN NEW.kind='runtime.action.succeeded' BEGIN
    UPDATE local_publication_launch_epoch SET generation=generation+1;
END;
CREATE TRIGGER IF NOT EXISTS publication_launch_delete AFTER DELETE ON local_publication_claims
WHEN OLD.kind='runtime.action.succeeded' BEGIN
    UPDATE local_publication_launch_epoch SET generation=generation+1;
END;
CREATE TRIGGER IF NOT EXISTS publication_launch_update AFTER UPDATE ON local_publication_claims
WHEN OLD.kind='runtime.action.succeeded' OR NEW.kind='runtime.action.succeeded' BEGIN
    UPDATE local_publication_launch_epoch SET generation=generation+1;
END;
CREATE TRIGGER IF NOT EXISTS publication_local_launch_insert AFTER INSERT ON local_publication_starts BEGIN
    UPDATE local_publication_launch_epoch SET generation=generation+1;
END;
CREATE TRIGGER IF NOT EXISTS publication_local_launch_delete AFTER DELETE ON local_publication_starts BEGIN
    UPDATE local_publication_launch_epoch SET generation=generation+1;
END;
CREATE TRIGGER IF NOT EXISTS publication_local_launch_update AFTER UPDATE ON local_publication_starts BEGIN
    UPDATE local_publication_launch_epoch SET generation=generation+1;
END;
CREATE TRIGGER IF NOT EXISTS publication_launch_definition_delete AFTER DELETE ON claims
WHEN OLD.kind='intent.desired' BEGIN
    UPDATE local_publication_launch_epoch SET generation=generation+1;
END;
CREATE TRIGGER IF NOT EXISTS publication_launch_definition_update
AFTER UPDATE OF subject,kind,body ON claims WHEN OLD.kind='intent.desired' BEGIN
    UPDATE local_publication_launch_epoch SET generation=generation+1;
END;
CREATE TRIGGER IF NOT EXISTS publication_claim_insert AFTER INSERT ON claims
WHEN NEW.kind IN ('runtime.observed','runtime.action.succeeded') BEGIN
    INSERT OR IGNORE INTO local_publication_pending VALUES(NEW.id);
END;
CREATE TRIGGER IF NOT EXISTS publication_claim_delete AFTER DELETE ON claims BEGIN
    DELETE FROM local_publication_claims WHERE claim_id=OLD.id;
    DELETE FROM local_publication_pending WHERE claim_id=OLD.id;
    INSERT INTO local_publication_batches
      SELECT OLD.batch_id,0 WHERE EXISTS(SELECT 1 FROM claims INDEXED BY claims_batch_index WHERE batch_id=OLD.batch_id LIMIT 1)
      ON CONFLICT(batch_id) DO UPDATE SET cursor=0;
END;
CREATE TRIGGER IF NOT EXISTS publication_claim_update
AFTER UPDATE OF subject,kind,origin,body,accepted_at_unix_ms,batch_id,store_index ON claims
WHEN OLD.kind IN ('runtime.observed','runtime.action.succeeded')
  OR NEW.kind IN ('runtime.observed','runtime.action.succeeded') BEGIN
    DELETE FROM local_publication_claims WHERE claim_id=OLD.id;
    INSERT OR IGNORE INTO local_publication_pending VALUES(NEW.id);
END;
CREATE TRIGGER IF NOT EXISTS publication_local_insert AFTER INSERT ON local_observations
WHEN NEW.kind='runtime.action.succeeded' AND json_extract(NEW.body,'$.fields.action')='start' BEGIN
    INSERT OR REPLACE INTO local_publication_starts VALUES
        (NEW.id,NEW.subject,COALESCE(json_extract(NEW.body,'$.fields.incarnation_id'),''));
END;
CREATE TRIGGER IF NOT EXISTS publication_local_delete AFTER DELETE ON local_observations BEGIN
    DELETE FROM local_publication_starts WHERE observation_id=OLD.id;
END;
CREATE TRIGGER IF NOT EXISTS publication_local_update
AFTER UPDATE OF subject,kind,body ON local_observations BEGIN
    DELETE FROM local_publication_starts WHERE observation_id=OLD.id;
    INSERT INTO local_publication_starts
    SELECT NEW.id,NEW.subject,COALESCE(json_extract(NEW.body,'$.fields.incarnation_id'),'')
    WHERE NEW.kind='runtime.action.succeeded' AND json_extract(NEW.body,'$.fields.action')='start';
END;
CREATE TRIGGER IF NOT EXISTS publication_batch_update
AFTER UPDATE OF origin,replica_sequence ON batches BEGIN
    INSERT OR IGNORE INTO local_publication_pending
    SELECT id FROM claims INDEXED BY claims_batch_index WHERE batch_id=NEW.id
      AND kind IN ('runtime.observed','runtime.action.succeeded');
END;
CREATE TRIGGER IF NOT EXISTS publication_record_insert AFTER INSERT ON replica_records BEGIN
    INSERT OR IGNORE INTO local_publication_pending SELECT id FROM claims WHERE id=NEW.claim_id
      AND kind IN ('runtime.observed','runtime.action.succeeded');
END;
CREATE TRIGGER IF NOT EXISTS publication_record_delete AFTER DELETE ON replica_records BEGIN
    INSERT OR IGNORE INTO local_publication_pending SELECT id FROM claims WHERE id=OLD.claim_id
      AND kind IN ('runtime.observed','runtime.action.succeeded');
END;
CREATE TRIGGER IF NOT EXISTS publication_record_update
AFTER UPDATE OF claim_id,position,state ON replica_records BEGIN
    INSERT OR IGNORE INTO local_publication_pending SELECT id FROM claims WHERE id IN (OLD.claim_id,NEW.claim_id)
      AND kind IN ('runtime.observed','runtime.action.succeeded');
END;

"#;

const REPAIR_SCHEMA: &str = r#"
CREATE TRIGGER IF NOT EXISTS publication_repair_insert AFTER INSERT ON projection_digest_repaired_claims BEGIN
    DELETE FROM local_publication_claims WHERE claim_id=NEW.id;
    DELETE FROM local_publication_pending WHERE claim_id=NEW.id;
    UPDATE local_publication_launch_epoch SET generation=generation+1
    WHERE EXISTS(SELECT 1 FROM claims WHERE id=NEW.id AND kind='intent.desired');
END;
CREATE TRIGGER IF NOT EXISTS publication_repair_delete AFTER DELETE ON projection_digest_repaired_claims BEGIN
    INSERT OR IGNORE INTO local_publication_pending SELECT id FROM claims WHERE id=OLD.id
      AND kind IN ('runtime.observed','runtime.action.succeeded');
    UPDATE local_publication_launch_epoch SET generation=generation+1
    WHERE EXISTS(SELECT 1 FROM claims WHERE id=OLD.id AND kind='intent.desired');
END;
"#;

pub(super) fn create_schema(connection: &Connection) -> Result<()> {
    connection.execute_batch(SCHEMA)?;
    let repairs:bool=connection.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='projection_digest_repaired_claims' AND type='table')",[],|row|row.get(0))?;
    if repairs {
        connection.execute_batch(REPAIR_SCHEMA)?;
    }
    Ok(())
}

/// Publish a process-lifetime cache invalidation hint after native commit resolution.
/// This is not authority for an effect. The publication writer reads its own evidence;
/// physical-effect authorization must independently fence the current declaration. Read failure invalidates every cached proof.
pub(super) fn install_cache_observer(graph: &GraphStore, runtime: &Arc<SmalltalkRuntime>) {
    let weak = Arc::downgrade(runtime);
    let observer = graph.connection.observe_commits(move |connection| {
        if let Some(runtime) = weak.upgrade() {
            *runtime
                .publication_cache_epoch
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = cache_epoch(connection).ok();
        }
    });
    *runtime
        .publication_cache_observer
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = Some(observer);
    // Registration precedes the fresh writer cut, so no intervening commit can be missed.
    let connection = graph.connection.write();
    *runtime
        .publication_cache_epoch
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = cache_epoch(&connection).ok();
}

fn cache_epoch(connection: &Connection) -> Result<(i64, bool)> {
    Ok(connection.prepare_cached(
        "SELECT generation,COALESCE((SELECT phase=3 FROM local_publication_bootstrap WHERE singleton=1),0)
            AND NOT EXISTS(SELECT 1 FROM local_publication_pending)
            AND NOT EXISTS(SELECT 1 FROM local_publication_batches)
         FROM local_publication_launch_epoch WHERE singleton=1"
    )?.query_row([], |row| Ok((row.get(0)?,row.get(1)?)))?)
}

/// Indexed ceilings only. Never reads historical bodies or advances a bootstrap page in open.
pub(super) fn initialize(transaction: &Transaction<'_>) -> Result<()> {
    transaction.execute(
        "INSERT OR IGNORE INTO local_publication_bootstrap(singleton,phase,cursor,ceiling,local_ceiling,progress_ms)
         SELECT 1,CASE WHEN shared=0 AND local=0 THEN 3 ELSE 0 END,0,shared,local,?1 FROM
         (SELECT COALESCE((SELECT MAX(store_index) FROM claims),0) AS shared,
                 COALESCE((SELECT MAX(id) FROM local_observations),0) AS local)",
        [now_ms().to_string()],
    )?;
    Ok(())
}

fn sync(transaction: &Transaction<'_>, id: &str) -> Result<()> {
    sync_from_source(transaction, transaction, id)
}

fn sync_from_source(transaction: &Transaction<'_>, source: &Connection, id: &str) -> Result<()> {
    let query = format!(
        "SELECT subject,origin,kind,body FROM claims
         WHERE id=?1 AND kind IN ('runtime.observed','runtime.action.succeeded') AND {UNREPAIRED_CLAIM}"
    );
    let row: Option<(String, String, String, String)> = source
        .prepare_cached(&query)?
        .query_row([id], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .optional()?;
    let Some((subject, origin, kind, body)) = row else {
        transaction.execute(
            "DELETE FROM local_publication_claims WHERE claim_id=?1",
            [id],
        )?;
        return Ok(());
    };
    let body: Value = serde_json::from_str(&body)?;
    let fields = body.get("fields").unwrap_or(&body);
    if kind == "runtime.action.succeeded" && fields["action"] != "start" {
        transaction.execute(
            "DELETE FROM local_publication_claims WHERE claim_id=?1",
            [id],
        )?;
        return Ok(());
    }
    let key = canonical::sortable_key(&canonical::claim_key(source, id)?);
    transaction.execute(
        "INSERT INTO local_publication_claims VALUES(?1,?2,?3,?4,?5,?6)
         ON CONFLICT(claim_id) DO UPDATE SET subject=excluded.subject,origin=excluded.origin,
             kind=excluded.kind,incarnation=excluded.incarnation,sort_key=excluded.sort_key
         WHERE local_publication_claims.subject<>excluded.subject
            OR local_publication_claims.origin<>excluded.origin
            OR local_publication_claims.kind<>excluded.kind
            OR local_publication_claims.incarnation<>excluded.incarnation
            OR local_publication_claims.sort_key<>excluded.sort_key",
        params![
            id,
            subject,
            origin,
            kind,
            fields["incarnation_id"].as_str().unwrap_or_default(),
            key
        ],
    )?;
    Ok(())
}

/// Bound every writer loan. A larger admission leaves a pending queue and Unknown;
/// the commit-driven background worker finishes it without a history fold or polling.
pub(super) fn flush(transaction: &Transaction<'_>) -> Result<()> {
    let started = std::time::Instant::now();
    // Deleting a legacy batch member changes later fallback canonical positions.
    // Coalesce that invalidation per batch, then repair it in bounded durable pages.
    // Never rescan the remaining batch in each row's delete trigger.
    let batch: Option<(String, i64)> = transaction.prepare_cached(
        "SELECT batch_id,cursor FROM local_publication_batches WHERE batch_id>='' ORDER BY batch_id LIMIT 1"
    )?.query_row([], |row| Ok((row.get(0)?,row.get(1)?))).optional()?;
    if let Some((batch_id, cursor)) = batch {
        let rows = transaction
            .prepare_cached(
                "SELECT store_index,id,kind FROM claims INDEXED BY claims_batch_index
             WHERE batch_id=?1 AND store_index>?2 ORDER BY store_index LIMIT ?3",
            )?
            .query_map(params![batch_id, cursor, PAGE], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut through = cursor;
        let mut processed = 0;
        for (index, id, kind) in &rows {
            if processed > 0 && started.elapsed() >= std::time::Duration::from_millis(10) {
                break;
            }
            if matches!(
                kind.as_str(),
                "runtime.observed" | "runtime.action.succeeded"
            ) {
                sync(transaction, id)?;
            }
            through = *index;
            processed += 1;
        }
        if processed == rows.len() && rows.len() < PAGE {
            transaction.execute(
                "DELETE FROM local_publication_batches WHERE batch_id=?1",
                [&batch_id],
            )?;
        } else {
            transaction.execute(
                "UPDATE local_publication_batches SET cursor=?2 WHERE batch_id=?1",
                params![batch_id, through],
            )?;
        }
        return Ok(());
    }
    let pending = transaction
        .prepare_cached("SELECT claim_id FROM local_publication_pending WHERE claim_id>='' ORDER BY claim_id LIMIT ?1")?
        .query_map([PAGE], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (offset, id) in pending.iter().enumerate() {
        if offset > 0 && started.elapsed() >= std::time::Duration::from_millis(10) {
            break;
        }
        sync(transaction, id)?;
        transaction.execute(
            "DELETE FROM local_publication_pending WHERE claim_id=?1",
            [id],
        )?;
    }
    Ok(())
}

pub(super) fn ready(connection: &Connection) -> Result<bool> {
    Ok(connection
        .prepare_cached(
            "SELECT COALESCE((SELECT phase=3 FROM local_publication_bootstrap WHERE singleton=1),0)
            AND NOT EXISTS(SELECT 1 FROM local_publication_pending)
            AND NOT EXISTS(SELECT 1 FROM local_publication_batches)",
        )?
        .query_row([], |row| row.get(0))?)
}

fn backfill(transaction: &Transaction<'_>) -> Result<()> {
    let started = std::time::Instant::now();
    let (phase, cursor, ceiling, subject, kind, local_id, local_ceiling): (
        u8,
        u64,
        u64,
        String,
        String,
        i64,
        i64,
    ) = transaction.query_row(
        "SELECT phase,cursor,ceiling,local_subject,local_kind,local_id,local_ceiling
         FROM local_publication_bootstrap WHERE singleton=1",
        [],
        |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
            ))
        },
    )?;
    if phase < 2 {
        let source_kind = if phase == 0 {
            "runtime.observed"
        } else {
            "runtime.action.succeeded"
        };
        let batch = transaction
            .prepare_cached(
                "SELECT store_index,id FROM claims INDEXED BY claims_kind_index
             WHERE kind=?1 AND store_index>?2 AND store_index<=?3 ORDER BY store_index LIMIT ?4",
            )?
            .query_map(params![source_kind, cursor, ceiling, PAGE], |row| {
                Ok((row.get::<_, u64>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut through = cursor;
        let mut processed = 0;
        for (index, id) in &batch {
            if processed > 0 && started.elapsed() >= std::time::Duration::from_millis(10) {
                break;
            }
            sync(transaction, id)?;
            through = *index;
            processed += 1;
        }
        let next = processed == batch.len() && batch.len() < PAGE;
        transaction.execute("UPDATE local_publication_bootstrap SET phase=?1,cursor=?2,progress_ms=?3 WHERE singleton=1",
            params![phase+u8::from(next),if next {0}else{through},now_ms().to_string()])?;
    } else if phase == 2 {
        // There is no kind-leading local index. Walk its existing covering index in
        // bounded pages; fetch bodies only for action-success rows, never other kinds.
        let batch=transaction.prepare_cached(
            "SELECT subject,kind,id FROM local_observations INDEXED BY local_observations_subject_kind_index
             WHERE (subject,kind,id)>(?1,?2,?3) ORDER BY subject,kind,id LIMIT ?4"
        )?.query_map(params![subject,kind,local_id,PAGE],|row|Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,i64>(2)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut through = (subject, kind, local_id);
        let mut processed = 0;
        for (subject, kind, id) in &batch {
            if processed > 0 && started.elapsed() >= std::time::Duration::from_millis(10) {
                break;
            }
            if kind == "runtime.action.succeeded" && *id <= local_ceiling {
                transaction.execute(
                    "INSERT OR REPLACE INTO local_publication_starts
                     SELECT id,subject,COALESCE(json_extract(body,'$.fields.incarnation_id'),'')
                     FROM local_observations WHERE id=?1 AND json_extract(body,'$.fields.action')='start'",[id])?;
            }
            through = (subject.clone(), kind.clone(), *id);
            processed += 1;
        }
        transaction.execute("UPDATE local_publication_bootstrap SET phase=?1,local_subject=?2,local_kind=?3,local_id=?4,progress_ms=?5 WHERE singleton=1",
            params![if processed==batch.len() && batch.len()<PAGE {3}else{2},through.0,through.1,through.2,now_ms().to_string()])?;
    }
    Ok(())
}

impl Store {
    pub fn publication_evidence_ready(&self) -> Result<bool> {
        ready(&self.readers.get())
    }

    /// At most 64 historical source rows and a 10ms processing budget per loan.
    /// The caller yields between jobs and stops on failure; the durable cursor resumes on reopen.
    pub fn advance_publication_evidence(&self) -> Result<bool> {
        let mut connection = self.connection.write_background();
        let transaction = connection.transaction()?;
        let pending: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM local_publication_pending) OR EXISTS(SELECT 1 FROM local_publication_batches)",
            [],
            |row| row.get(0),
        )?;
        // A loan does one bounded task. Exact deltas drain in runtime maintenance at commit;
        // otherwise this page touches only derived tables and creates no pending source delta.
        if !pending {
            backfill(&transaction)?;
        }
        transaction.commit()?;
        ready(&connection)
    }
}

pub(super) fn reset(transaction: &Transaction<'_>) -> Result<()> {
    transaction.execute_batch(
        "DELETE FROM local_publication_claims;
        DELETE FROM local_publication_starts; DELETE FROM local_publication_pending; DELETE FROM local_publication_batches;
        DELETE FROM local_publication_bootstrap;",
    )?;
    initialize(transaction)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(kind: &str, incarnation: &str) -> ClaimInput {
        ClaimInput {
            subject: "agent/example/worker".into(),
            kind: kind.into(),
            actor: Some("agent/example/writer".into()),
            fields: if kind == "runtime.observed" {
                BTreeMap::from([
                    ("status".into(), json!("running")),
                    ("incarnation_id".into(), json!(incarnation)),
                ])
            } else {
                BTreeMap::from([
                    ("action".into(), json!("start")),
                    ("incarnation_id".into(), json!(incarnation)),
                    ("desired_token".into(), json!("not-retained")),
                ])
            },
            evidence: vec![],
            expected_subject: None,
            idempotency_key: None,
        }
    }
    fn finish(store: &Store) {
        for _ in 0..10000 {
            if store.advance_publication_evidence().unwrap() {
                return;
            }
        }
        panic!("bootstrap did not finish");
    }
    fn oracle(store: &Store) {
        assert!(store.publication_evidence_ready().unwrap());
        let connection = store.readers.get();
        let query = format!(
            "SELECT id,subject,origin,kind,body FROM claims WHERE kind IN
            ('runtime.observed','runtime.action.succeeded') AND {UNREPAIRED_CLAIM}"
        );
        let rows = connection
            .prepare(&query)
            .unwrap()
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        let mut expected = Vec::new();
        for (id, subject, origin, kind, body) in rows {
            let body: Value = serde_json::from_str(&body).unwrap();
            let fields = body.get("fields").unwrap_or(&body);
            if kind == "runtime.action.succeeded" && fields["action"] != "start" {
                continue;
            }
            expected.push((
                id.clone(),
                subject,
                origin,
                kind,
                fields["incarnation_id"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
                canonical::sortable_key(&canonical::claim_key(&connection, &id).unwrap()),
            ));
        }
        expected.sort();
        let actual = connection
            .prepare(
                "SELECT claim_id,subject,origin,kind,incarnation,sort_key
            FROM local_publication_claims ORDER BY claim_id",
            )
            .unwrap()
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Vec<u8>>(5)?,
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(actual, expected);
        let local = connection
            .prepare(
                "SELECT id,subject,COALESCE(json_extract(body,'$.fields.incarnation_id'),'')
            FROM local_observations WHERE kind='runtime.action.succeeded'
            AND json_extract(body,'$.fields.action')='start' ORDER BY id",
            )
            .unwrap()
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        let actual = connection
            .prepare(
                "SELECT observation_id,subject,incarnation FROM local_publication_starts
            ORDER BY observation_id",
            )
            .unwrap()
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(actual, local);
    }

    #[test]
    #[ignore = "frozen census URI required; writes only a new scratch file; prints aggregates"]
    fn publication_facts_census_source_bootstrap() {
        let uri = std::env::var("APPLY_RESTART_CENSUS_SOURCE")
            .expect("immutable read-only frozen census URI");
        assert!(uri.contains("immutable=1") && uri.contains("mode=ro"));
        let source = Connection::open_with_flags(
            uri,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
        )
        .unwrap();
        source.pragma_update(None, "query_only", true).unwrap();
        let root = tempfile::tempdir().unwrap();
        let mut scratch = Connection::open(root.path().join("facts.db")).unwrap();
        // Match the native writer's WAL/FULL settings; DELETE-journal commits do not
        // measure the supported bootstrap path's writer loans.
        scratch
            .execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;")
            .unwrap();
        scratch.execute_batch("CREATE TABLE claims(id TEXT,subject TEXT,kind TEXT,origin TEXT,body TEXT,accepted_at_unix_ms TEXT,batch_id TEXT,store_index INTEGER);
            CREATE INDEX claims_batch_index ON claims(batch_id,store_index);
            CREATE TABLE batches(id TEXT,origin TEXT,replica_sequence INTEGER);
            CREATE TABLE replica_records(claim_id TEXT,position INTEGER,state TEXT);
            CREATE TABLE local_observations(id INTEGER,subject TEXT,kind TEXT,body TEXT);").unwrap();
        let started = std::time::Instant::now();
        create_schema(&scratch).unwrap();
        println!(
            "publication census source schema_seconds={}",
            started.elapsed().as_secs_f64()
        );
        for kind in ["runtime.observed", "runtime.action.succeeded"] {
            let started = std::time::Instant::now();
            let mut cursor = 0_u64;
            let mut count = 0;
            let mut holds = Vec::new();
            loop {
                let batch = source
                    .prepare_cached(
                        "SELECT store_index,id FROM claims INDEXED BY claims_kind_index
                    WHERE kind=?1 AND store_index>?2 ORDER BY store_index LIMIT ?3",
                    )
                    .unwrap()
                    .query_map(params![kind, cursor, PAGE], |row| {
                        Ok((row.get::<_, u64>(0)?, row.get::<_, String>(1)?))
                    })
                    .unwrap()
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .unwrap();
                if batch.is_empty() {
                    break;
                }
                let page = std::time::Instant::now();
                let tx = scratch.transaction().unwrap();
                for (processed, (index, id)) in batch.iter().enumerate() {
                    if processed > 0 && page.elapsed() >= std::time::Duration::from_millis(10) {
                        break;
                    }
                    sync_from_source(&tx, &source, id).unwrap();
                    cursor = *index;
                    count += 1;
                }
                tx.commit().unwrap();
                holds.push(page.elapsed().as_micros());
            }
            holds.sort();
            println!(
                "publication census source kind={kind} indexed_source_rows={count} elapsed_seconds={} pages={} writer_hold_p95_us={} writer_hold_max_us={}",
                started.elapsed().as_secs_f64(),
                holds.len(),
                holds[holds.len() * 95 / 100],
                holds.last().unwrap()
            );
        }
        let version: String = source
            .query_row("SELECT sqlite_version()", [], |row| row.get(0))
            .unwrap();
        let facts: u64 = scratch
            .query_row("SELECT count(*) FROM local_publication_claims", [], |row| {
                row.get(0)
            })
            .unwrap();
        println!("publication census source SQLite={version} retained_shared_facts={facts}");
    }

    #[test]
    #[ignore = "private census scratch path required; run alone and print aggregates only"]
    fn publication_facts_census_bootstrap() {
        let path = std::env::var_os("APPLY_RESTART_CENSUS_SCRATCH")
            .expect("private matching-row scratch path");
        // Repeated probes must start from source rows, not a previously completed cursor.
        // Copy only the matching-row fixture, retaining its existing source indexes.
        let root = tempfile::tempdir().unwrap();
        let mut connection = Connection::open(root.path().join("facts.db")).unwrap();
        connection
            .execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;")
            .unwrap();
        let uri = format!("file:{}?mode=ro", std::path::Path::new(&path).display());
        connection
            .execute("ATTACH DATABASE ?1 AS census", [uri])
            .unwrap();
        let copy_started = std::time::Instant::now();
        let schemas = connection.prepare("SELECT sql FROM census.sqlite_schema
            WHERE type IN ('table','index') AND sql IS NOT NULL AND tbl_name IN
                ('claims','batches','replica_records','local_observations','projection_digest_repaired_claims')
            ORDER BY type='index'").unwrap().query_map([],|row|row.get::<_,String>(0)).unwrap()
            .collect::<rusqlite::Result<Vec<_>>>().unwrap();
        for sql in schemas {
            connection.execute_batch(&sql).unwrap();
        }
        let tx = connection.transaction().unwrap();
        for table in [
            "claims",
            "batches",
            "replica_records",
            "local_observations",
            "projection_digest_repaired_claims",
        ] {
            tx.execute_batch(&format!(
                "INSERT INTO main.{table} SELECT * FROM census.{table}"
            ))
            .unwrap();
        }
        tx.commit().unwrap();
        connection.execute_batch("DETACH DATABASE census").unwrap();
        println!(
            "publication census matching_fixture_copy_seconds={}",
            copy_started.elapsed().as_secs_f64()
        );
        let source_version: String = connection
            .query_row("SELECT sqlite_version()", [], |row| row.get(0))
            .unwrap();
        let started = std::time::Instant::now();
        create_schema(&connection).unwrap();
        let schema_seconds = started.elapsed().as_secs_f64();
        let tx = connection.transaction().unwrap();
        initialize(&tx).unwrap();
        tx.commit().unwrap();
        let started = std::time::Instant::now();
        let mut pages = 0;
        let mut holds = Vec::new();
        loop {
            let page_started = std::time::Instant::now();
            let tx = connection.transaction().unwrap();
            backfill(&tx).unwrap();
            flush(&tx).unwrap();
            let done = ready(&tx).unwrap();
            tx.commit().unwrap();
            holds.push(page_started.elapsed().as_micros());
            pages += 1;
            if done {
                break;
            }
        }
        let total = started.elapsed().as_secs_f64();
        holds.sort();
        let shared: u64 = connection
            .query_row("SELECT count(*) FROM local_publication_claims", [], |row| {
                row.get(0)
            })
            .unwrap();
        let local: u64 = connection
            .query_row("SELECT count(*) FROM local_publication_starts", [], |row| {
                row.get(0)
            })
            .unwrap();
        println!(
            "publication census SQLite={source_version} schema_seconds={schema_seconds} bootstrap_seconds={total} pages={pages} shared_rows={shared} local_rows={local} writer_hold_p95_us={} writer_hold_max_us={}",
            holds[holds.len() * 95 / 100],
            holds.last().unwrap()
        );
        let bytes:u64=connection.query_row("SELECT sum(pgsize) FROM dbstat WHERE name IN
            ('local_publication_claims','local_publication_claims_lookup','local_publication_runtime_lookup',
             'local_publication_starts','local_publication_starts_lookup','local_publication_pending','local_publication_bootstrap','local_publication_launch_epoch','local_publication_batches')",[],|row|row.get(0)).unwrap();
        println!("publication census derived_bytes={bytes}");
    }

    #[test]
    #[ignore = "private matching census fixture; native background/foreground contention; aggregates only"]
    fn publication_facts_native_foreground_wait_during_census_bootstrap() {
        let source =
            std::env::var_os("APPLY_RESTART_CENSUS_SCRATCH").expect("private matching fixture");
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("foreground.db");
        let store = Arc::new(Store::open(&path, "node").unwrap());
        {
            let mut writer=store.connection.write();
            let uri=format!("file:{}?mode=ro",std::path::Path::new(&source).display());
            writer.execute("ATTACH DATABASE ?1 AS matching",[uri]).unwrap();
            let tx=writer.transaction().unwrap();
            // Native schema and native writer; matching bodies and all canonical ordering
            // inputs are preserved. Unused envelope hash/raw/predecessor/time fields are
            // synthetic. This measures foreground admission, not replay/admission/health.
            tx.execute_batch("INSERT INTO main.batches(id,origin,replica_sequence,hash,accepted_at_unix_ms)
                  SELECT id,origin,replica_sequence,'fixture','0' FROM matching.batches;
                INSERT INTO main.claims(store_index,id,batch_id,subject,origin,kind,body,predecessors,accepted_at_unix_ms)
                  SELECT store_index,id,batch_id,subject,origin,kind,body,'[]',accepted_at_unix_ms FROM matching.claims;
                INSERT INTO main.replica_records(record_ref,writer,sequence,envelope_hash,position,raw,state,claim_id,updated_at_unix_ms)
                  SELECT 'fixture:'||source.rowid,batch.origin,batch.replica_sequence,'fixture:'||source.rowid,source.position,X'',source.state,source.claim_id,'0'
                  FROM matching.replica_records source JOIN matching.claims claim ON claim.id=source.claim_id
                  JOIN matching.batches batch ON batch.id=claim.batch_id;
                INSERT INTO main.local_observations(id,subject,kind,body,after_store_index,observed_at_unix_ms)
                  SELECT id,subject,kind,body,after_store_index,0 FROM matching.local_observations;").unwrap();
            reset(&tx).unwrap();
            tx.commit().unwrap();
            writer.execute_batch("DETACH DATABASE matching").unwrap();
        }
        assert!(!store.publication_evidence_ready().unwrap());
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_done = done.clone();
        let worker_store = store.clone();
        struct BootstrapDone(Arc<std::sync::atomic::AtomicBool>);
        impl Drop for BootstrapDone {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::Release);
            }
        }
        let worker = std::thread::spawn(move || {
            let _finished = BootstrapDone(worker_done.clone());
            let start = std::time::Instant::now();
            let mut pages = 0;
            loop {
                let complete = worker_store.advance_publication_evidence().unwrap();
                pages += 1;
                if complete {
                    break;
                }
                // Production releases the same background guard and yields between pages.
                std::thread::yield_now();
            }
            worker_done.store(true, std::sync::atomic::Ordering::Release);
            (pages, start.elapsed().as_secs_f64())
        });
        let mut waits = Vec::new();
        let mut holds = Vec::new();
        let mut totals = Vec::new();
        while !done.load(std::sync::atomic::Ordering::Acquire) {
            let started = std::time::Instant::now();
            let mut writer = store.connection.write();
            let wait = started.elapsed().as_micros();
            let acquired = std::time::Instant::now();
            let tx = writer.transaction().unwrap();
            let input = ClaimInput {
                subject: "agent/example/latency".into(),
                kind: "harness.diagnostic".into(),
                actor: None,
                fields: BTreeMap::from([
                    ("severity".into(), json!("info")),
                    ("code".into(), json!("bootstrap-contention-probe")),
                    (
                        "reason".into(),
                        json!(format!("foreground-{}", waits.len())),
                    ),
                ]),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            };
            insert_local_observation_tx(&tx, "node", &input, now_ms()).unwrap();
            tx.commit().unwrap();
            drop(writer);
            waits.push(wait);
            holds.push(acquired.elapsed().as_micros());
            totals.push(started.elapsed().as_micros());
        }
        let (pages, seconds) = worker.join().unwrap();
        assert!(!waits.is_empty());
        let samples = waits.len();
        waits.sort();
        holds.sort();
        totals.sort();
        println!(
            "publication foreground contention samples={samples} bootstrap_pages={pages} bootstrap_seconds={seconds} wait_p95_us={} wait_max_us={} hold_p95_us={} hold_max_us={} total_p95_us={} total_max_us={}",
            waits[samples * 95 / 100],
            waits.last().unwrap(),
            holds[samples * 95 / 100],
            holds.last().unwrap(),
            totals[samples * 95 / 100],
            totals.last().unwrap()
        );
        assert!(store.publication_evidence_ready().unwrap());
    }

    #[test]
    #[ignore = "private native measurements: run alone with --exact --ignored --test-threads=1 --nocapture"]
    fn publication_facts_native_write_cost() {
        use smallclaims::sqlite::work;
        for after in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("scratch.db");
            let store = if after {
                None
            } else {
                Some(
                    GraphStore::open(&path, "node", Arc::new(SmalltalkRuntime::default())).unwrap(),
                )
            };
            let revised = if after {
                Some(Store::open(&path, "node").unwrap())
            } else {
                None
            };
            let graph: &GraphStore = store
                .as_ref()
                .unwrap_or_else(|| &revised.as_ref().unwrap().graph);
            if !after {
                let names=graph.readers.get().prepare("SELECT name FROM sqlite_schema WHERE type='trigger' AND name LIKE 'publication_%'")
                    .unwrap().query_map([],|row|row.get::<_,String>(0)).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap();
                let connection = graph.connection.write();
                for name in names {
                    connection
                        .execute_batch(&format!("DROP TRIGGER {name}"))
                        .unwrap();
                }
            }
            for (kind, local) in [
                ("runtime.observed", false),
                ("runtime.action.succeeded", true),
                ("runtime.action.succeeded", false),
            ] {
                let mut elapsed = Vec::new();
                let mut total = work::SqliteWork::default();
                for i in 0..100 {
                    let input = input(kind, &format!("cost-{kind}-{local}-{i}"));
                    let mut connection = graph.connection.write();
                    let before = work::total();
                    let started = std::time::Instant::now();
                    let tx = connection.transaction().unwrap();
                    if local {
                        insert_local_observation_tx(&tx, "node", &input, now_ms()).unwrap();
                    } else {
                        append_claim_tx(
                            &tx,
                            "node",
                            &input.subject,
                            kind,
                            input.actor.as_deref(),
                            &json!({"fields":input.fields}),
                            &[],
                            None,
                        )
                        .unwrap();
                    }
                    tx.commit().unwrap();
                    drop(connection);
                    elapsed.push(started.elapsed().as_nanos());
                    let cost = work::total() - before;
                    total.vm_steps += cost.vm_steps;
                    total.statements += cost.statements;
                }
                elapsed.sort();
                println!(
                    "publication write after={after} kind={kind} local={local}: per insert vm_steps={} statements={} writer_hold_median_us={} writer_hold_p95_us={} writer_hold_max_us={}",
                    total.vm_steps / 100,
                    total.statements / 100,
                    elapsed[50] / 1000,
                    elapsed[95] / 1000,
                    elapsed[99] / 1000
                );
            }
        }
    }

    #[test]
    #[ignore = "native delete counters: run alone with --ignored --test-threads=1 --nocapture"]
    fn publication_facts_native_delete_cost() {
        use smallclaims::sqlite::work;
        for after in [false, true] {
            for (batch_size, total_rows) in [(1, 100), (1000, 1000)] {
                for runtime in [false, true] {
                    let root = tempfile::tempdir().unwrap();
                    let path = root.path().join("delete.db");
                    let baseline = (!after).then(|| {
                        GraphStore::open(&path, "node", Arc::new(SmalltalkRuntime::default()))
                            .unwrap()
                    });
                    let revised = after.then(|| Store::open(&path, "node").unwrap());
                    let graph = baseline
                        .as_ref()
                        .unwrap_or_else(|| &revised.as_ref().unwrap().graph);
                    if !after {
                        let names=graph.readers.get().prepare("SELECT name FROM sqlite_schema WHERE type='trigger' AND name LIKE 'publication_%'").unwrap()
                            .query_map([],|row|row.get::<_,String>(0)).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap();
                        let writer = graph.connection.write();
                        for name in names {
                            writer
                                .execute_batch(&format!("DROP TRIGGER {name}"))
                                .unwrap();
                        }
                    }
                    let mut ids = Vec::new();
                    {
                        let mut writer = graph.connection.write();
                        let tx = writer.transaction().unwrap();
                        let mut batch = None;
                        for i in 0..total_rows {
                            let kind = if runtime {
                                "runtime.observed"
                            } else {
                                "runtime.action.requested"
                            };
                            let fields = if runtime {
                                json!({"status":"running","incarnation_id":format!("delete-{i}")})
                            } else {
                                json!({"action":"screen","reason":format!("delete-{i}")})
                            };
                            let row = append_claim_tx(
                                &tx,
                                "node",
                                "agent/example/worker",
                                kind,
                                None,
                                &json!({"fields":fields}),
                                &[],
                                batch.as_deref(),
                            )
                            .unwrap();
                            if batch_size > 1 {
                                batch = Some(row.batch_id);
                            }
                            ids.push(row.id);
                        }
                        tx.commit().unwrap();
                    }
                    if let Some(store) = &revised {
                        finish(store);
                    }
                    let mut costs = work::SqliteWork::default();
                    let mut holds = Vec::new();
                    let mut repair_cost = work::SqliteWork::default();
                    let mut repair_holds = Vec::new();
                    // One native delete statement per chunk, including maintenance and commit observer.
                    for chunk in ids.chunks(100) {
                        let mut writer = graph.connection.write();
                        let before = work::total();
                        let started = std::time::Instant::now();
                        let tx = writer.transaction().unwrap();
                        let arguments = std::iter::repeat_n("?", chunk.len())
                            .collect::<Vec<_>>()
                            .join(",");
                        tx.execute(
                            &format!("DELETE FROM claims WHERE id IN ({arguments})"),
                            rusqlite::params_from_iter(chunk),
                        )
                        .unwrap();
                        tx.commit().unwrap();
                        drop(writer);
                        holds.push(started.elapsed().as_micros());
                        let cost = work::total() - before;
                        costs.vm_steps += cost.vm_steps;
                        costs.statements += cost.statements;
                        if let Some(store) = &revised {
                            while !store.publication_evidence_ready().unwrap() {
                                let mut writer = graph.connection.write_background();
                                let before = work::total();
                                let started = std::time::Instant::now();
                                writer.transaction().unwrap().commit().unwrap();
                                drop(writer);
                                repair_holds.push(started.elapsed().as_micros());
                                let cost = work::total() - before;
                                repair_cost.vm_steps += cost.vm_steps;
                                repair_cost.statements += cost.statements;
                            }
                        }
                    }
                    holds.sort();
                    println!(
                        "publication delete after={after} runtime={runtime} batch_size={batch_size} rows={total_rows} chunk=100: vm_per_row={} statements={} chunk_hold_median_us={} chunk_hold_max_us={}",
                        costs.vm_steps / total_rows as u64,
                        costs.statements,
                        holds[holds.len() / 2],
                        holds.last().unwrap()
                    );
                    repair_holds.sort();
                    println!(
                        "publication delete repair after={after} runtime={runtime} batch_size={batch_size}: vm_per_deleted_row={} statements={} loans={} loan_hold_p95_us={} loan_hold_max_us={}",
                        repair_cost.vm_steps / total_rows as u64,
                        repair_cost.statements,
                        repair_holds.len(),
                        repair_holds
                            .get(repair_holds.len() * 95 / 100)
                            .copied()
                            .unwrap_or(0),
                        repair_holds.last().copied().unwrap_or(0)
                    );
                    if let Some(store) = &revised {
                        oracle(store);
                    }
                }
            }
        }
    }

    #[test]
    fn publication_facts_batch_deletion_coalesces_and_repairs_legacy_positions() {
        let store = Store::open_memory("node").unwrap();
        let mut ids = Vec::new();
        {
            let mut writer = store.connection.write();
            let tx = writer.transaction().unwrap();
            let mut batch = None;
            for i in 0..1000 {
                let row = append_claim_tx(
                    &tx,
                    "node",
                    "agent/example/worker",
                    "runtime.observed",
                    None,
                    &json!({"fields":{"status":"running","incarnation_id":format!("batch-{i}")}}),
                    &[],
                    batch.as_deref(),
                )
                .unwrap();
                batch = Some(row.batch_id);
                ids.push(row.id);
            }
            tx.commit().unwrap();
        }
        finish(&store);
        let mut writer = store.connection.write();
        let tx = writer.transaction().unwrap();
        for id in &ids[..500] {
            tx.execute("DELETE FROM claims WHERE id=?1", [id]).unwrap();
        }
        assert_eq!(
            tx.query_row(
                "SELECT count(*) FROM local_publication_batches",
                [],
                |row| row.get::<_, usize>(0)
            )
            .unwrap(),
            1
        );
        tx.commit().unwrap();
        drop(writer);
        assert!(!store.publication_evidence_ready().unwrap());
        finish(&store);
        oracle(&store);
    }

    #[test]
    fn publication_facts_native_checkpoint_trim_keeps_only_retained_sources() {
        let store = Store::open_memory("node").unwrap();
        for i in 0..4 {
            store
                .append_claim(&input("runtime.observed", &format!("inc-{i}")))
                .unwrap();
            // Legacy shared system receipts use the actor-null checkpoint slot. Modern
            // actor-null receipts are local; requester-authored shared receipts never drop.
            let mut writer = store.connection.write();
            let tx = writer.transaction().unwrap();
            append_claim_tx(
                &tx,
                "node",
                "agent/example/worker",
                "runtime.action.succeeded",
                None,
                &json!({"fields":{"action":"start","incarnation_id":"legacy",
                    "desired_token":"not-retained","reason":format!("receipt-{i}")}}),
                &[],
                None,
            )
            .unwrap();
            tx.commit().unwrap();
            drop(writer);
            // The newest envelope is protected independently of the kind's drop rule.
            store.seal_local_batches().unwrap();
        }
        let before: u64 = store
            .readers
            .get()
            .query_row("SELECT COUNT(*) FROM claims", [], |row| row.get(0))
            .unwrap();
        assert!(store.trim_checkpoint_for_test(now_ms() + 6 * 24 * 60 * 60 * 1000) > 0);
        finish(&store);
        oracle(&store);
        let after: u64 = store
            .readers
            .get()
            .query_row("SELECT COUNT(*) FROM claims", [], |row| row.get(0))
            .unwrap();
        assert!(after < before);
        assert!(
            store
                .readers
                .get()
                .query_row("SELECT COUNT(*) FROM checkpoint_claims", [], |row| row
                    .get::<_, u64>(0))
                .unwrap()
                > 0
        );
    }

    #[test]
    fn publication_facts_cover_native_append_local_retention_and_forget() {
        let store = Store::open_memory("node").unwrap();
        store
            .append_claim(&input("runtime.observed", "one"))
            .unwrap();
        store
            .append_claim(&input("runtime.action.succeeded", "one"))
            .unwrap();
        let mut local = input("runtime.action.succeeded", "local");
        local.actor = None;
        store.append_claim(&local).unwrap();
        oracle(&store);
        store.forget_current_views();
        oracle(&store);
        let mut connection = store.connection.write();
        let tx = connection.transaction().unwrap();
        tx.execute("UPDATE local_observations SET body=json_set(body,'$.fields.incarnation_id','replacement')",[]).unwrap();
        tx.commit().unwrap();
        drop(connection);
        oracle(&store);
        store
            .connection
            .write()
            .execute("DELETE FROM local_observations", [])
            .unwrap();
        oracle(&store);
    }

    #[test]
    fn publication_facts_replicated_admission_and_full_replay_match_recompute() {
        let source = Store::open_memory("writer").unwrap();
        source
            .append_claim(&input("runtime.observed", "one"))
            .unwrap();
        source
            .append_claim(&input("runtime.action.succeeded", "one"))
            .unwrap();
        let target = Store::open_memory("receiver").unwrap();
        target
            .import_replication("writer", &source.export_replication(0).unwrap())
            .unwrap();
        finish(&target);
        oracle(&target);
        target.replay_replication_graph().unwrap();
        assert!(!target.publication_evidence_ready().unwrap());
        finish(&target);
        oracle(&target);
    }

    #[test]
    fn publication_facts_native_record_repair_excludes_the_repaired_source() {
        let store = Store::open_memory("node").unwrap();
        let old = store
            .append_claim(&input("runtime.observed", "old"))
            .unwrap();
        let new = store
            .append_claim(&input("runtime.observed", "new"))
            .unwrap();
        store.seal_local_batches().unwrap();
        let record: String = store
            .readers
            .get()
            .query_row(
                "SELECT record_ref FROM replica_records WHERE claim_id=?1",
                [&old.id],
                |row| row.get(0),
            )
            .unwrap();
        // Reproduce a retained source whose envelope needs repair, then use the native
        // repair publication path. A receipt alone does not change record admission state.
        store
            .connection
            .write()
            .execute(
                "UPDATE replica_records SET state='invalid' WHERE record_ref=?1",
                [&record],
            )
            .unwrap();
        let source = format!(
            "version 2\nrepair {record:?} {{ replacement {:?}; reason \"synthetic native repair\"; }}",
            new.id
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
        store
            .apply(
                &intent,
                &preview.subject_tokens,
                "native-publication-repair",
            )
            .unwrap();
        finish(&store);
        oracle(&store);
        assert!(
            !store
                .readers
                .get()
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM local_publication_claims WHERE claim_id=?1)",
                    [&old.id],
                    |row| row.get::<_, bool>(0)
                )
                .unwrap()
        );
    }

    #[test]
    fn publication_facts_repair_retraction_and_checkpoint_source_deletion() {
        let store = Store::open_memory("node").unwrap();
        let a = store
            .append_claim(&input("runtime.observed", "one"))
            .unwrap();
        let b = store
            .append_claim(&input("runtime.action.succeeded", "one"))
            .unwrap();
        {
            let mut connection = store.connection.write();
            let tx = connection.transaction().unwrap();
            tx.execute(
                "INSERT INTO projection_digest_repaired_claims VALUES(?1)",
                [&a.id],
            )
            .unwrap();
            tx.commit().unwrap();
        }
        oracle(&store);
        {
            let mut connection = store.connection.write();
            let tx = connection.transaction().unwrap();
            tx.execute(
                "DELETE FROM projection_digest_repaired_claims WHERE id=?1",
                [&a.id],
            )
            .unwrap();
            tx.commit().unwrap();
        }
        oracle(&store);
        // Trimming and tombstoning delete this same source row; the tombstone never
        // reconstructs a missing launch body or declaration. Retained evidence is exact.
        {
            let mut connection = store.connection.write();
            let tx = connection.transaction().unwrap();
            tx.execute("DELETE FROM claims WHERE id=?1", [&b.id])
                .unwrap();
            tx.commit().unwrap();
        }
        oracle(&store);
        assert!(
            store
                .publication_launch("agent/example/worker", "one")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn publication_facts_late_canonical_claim_and_metadata_corrections_do_not_regress() {
        let store = Store::open_memory("node").unwrap();
        let first = store
            .append_claim(&input("runtime.observed", "first"))
            .unwrap();
        let late = store
            .append_claim(&input("runtime.observed", "late"))
            .unwrap();
        {
            let mut connection = store.connection.write();
            let tx = connection.transaction().unwrap();
            tx.execute(
                "UPDATE claims SET accepted_at_unix_ms='1' WHERE id=?1",
                [&late.id],
            )
            .unwrap();
            tx.execute(
                "UPDATE batches SET replica_sequence=replica_sequence+100 WHERE id=?1",
                [&first.batch_id],
            )
            .unwrap();
            tx.commit().unwrap();
        }
        oracle(&store);
        let connection = store.readers.get();
        let selected: String = connection
            .query_row(
                "SELECT claim_id FROM local_publication_claims
            WHERE kind='runtime.observed' ORDER BY sort_key DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(selected, first.id);
    }

    #[test]
    fn publication_facts_second_open_preserves_schema_cookie() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("reopen.db");
        let store = Store::open(&path, "node").unwrap();
        let cookie: i64 = store
            .readers
            .get()
            .query_row("PRAGMA schema_version", [], |row| row.get(0))
            .unwrap();
        drop(store);
        let reopened = Store::open(&path, "node").unwrap();
        assert_eq!(
            cookie,
            reopened
                .readers
                .get()
                .query_row::<i64, _, _>("PRAGMA schema_version", [], |row| row.get(0))
                .unwrap()
        );
        assert!(reopened.publication_evidence_ready().unwrap());
        // A genuinely older record trigger upgrades once; unchanged reopens do not
        // repeat that migration or leave the stale body installed.
        reopened.connection.write().execute_batch(
            "DROP TRIGGER directive_note_record_insert; CREATE TRIGGER directive_note_record_insert
             AFTER INSERT ON replica_records BEGIN SELECT 1; END;"
        ).unwrap();
        drop(reopened);
        let migrated = Store::open(&path, "node").unwrap();
        let definition: String = migrated
            .readers
            .get()
            .query_row(
                "SELECT sql FROM sqlite_master WHERE name='directive_note_record_insert'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(definition.contains("local_directive_note_dirty"));
        let migrated_cookie: i64 = migrated
            .readers
            .get()
            .query_row("PRAGMA schema_version", [], |row| row.get(0))
            .unwrap();
        drop(migrated);
        let stable = Store::open(&path, "node").unwrap();
        assert_eq!(
            migrated_cookie,
            stable
                .readers
                .get()
                .query_row::<i64, _, _>("PRAGMA schema_version", [], |row| row.get(0))
                .unwrap()
        );
    }

    #[test]
    fn publication_facts_bootstrap_is_durable_bounded_and_fail_closed() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("graph.db");
        let store = Store::open(&path, "node").unwrap();
        for _ in 0..(PAGE * 3) {
            store
                .append_claim(&input("runtime.observed", "one"))
                .unwrap();
        }
        {
            let mut connection = store.connection.write();
            let tx = connection.transaction().unwrap();
            reset(&tx).unwrap();
            tx.commit().unwrap();
        }
        drop(store);
        let store = Store::open(&path, "node").unwrap();
        assert!(!store.publication_evidence_ready().unwrap());
        let desired = crate::graph::parse_intent(
            "version 2\nagent \"example/worker\" { command \"true\"; }",
            "node",
        )
        .unwrap();
        let check = super::super::publication::check(
            &store.readers.get(),
            &desired.subjects["agent/example/worker"],
            u64::MAX,
            "node",
        )
        .unwrap();
        assert_eq!(
            check.interruption,
            crate::publication::Interruption::Unknown
        );
        assert_eq!(
            check.unknown_reason,
            Some("publication-evidence-incomplete")
        );
        assert!(!store.advance_publication_evidence().unwrap());
        let count: u64 = store
            .readers
            .get()
            .query_row("SELECT COUNT(*) FROM local_publication_claims", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(count > 0 && count <= PAGE as u64);
        let cursor: u64 = store
            .readers
            .get()
            .query_row(
                "SELECT cursor FROM local_publication_bootstrap",
                [],
                |row| row.get(0),
            )
            .unwrap();
        drop(store);
        let reopened = Store::open(&path, "node").unwrap();
        assert_eq!(
            cursor,
            reopened
                .readers
                .get()
                .query_row(
                    "SELECT cursor FROM local_publication_bootstrap",
                    [],
                    |row| row.get::<_, u64>(0)
                )
                .unwrap()
        );
        finish(&reopened);
        oracle(&reopened);
    }

    #[test]
    fn publication_facts_shuffled_deltas_equal_full_recompute() {
        // Deterministic property sweep: shuffled inserts, repairs, unrepaired restores,
        // updates, local trimming and source deletions across several canonical batches.
        for seed in 1..=8_u64 {
            let store = Store::open_memory("node").unwrap();
            let mut random = seed;
            let mut claims = Vec::new();
            for step in 0..80 {
                random = random.wrapping_mul(6364136223846793005).wrapping_add(1);
                match (random >> 32) % 7 {
                    0 | 1 => claims.push(
                        store
                            .append_claim(&input(
                                if step % 2 == 0 {
                                    "runtime.observed"
                                } else {
                                    "runtime.action.succeeded"
                                },
                                &format!("inc-{step}"),
                            ))
                            .unwrap(),
                    ),
                    2 => {
                        let mut local = input("runtime.action.succeeded", &format!("local-{step}"));
                        local.actor = None;
                        store.append_claim(&local).unwrap();
                    }
                    action => {
                        if let Some(claim) = claims.get((random as usize) % claims.len().max(1)) {
                            let mut connection = store.connection.write();
                            let tx = connection.transaction().unwrap();
                            match action {
                                3 => {
                                    tx.execute("INSERT OR IGNORE INTO projection_digest_repaired_claims VALUES(?1)",[&claim.id]).unwrap();
                                }
                                4 => {
                                    tx.execute(
                                        "DELETE FROM projection_digest_repaired_claims WHERE id=?1",
                                        [&claim.id],
                                    )
                                    .unwrap();
                                }
                                5 => {
                                    tx.execute(
                                        "UPDATE claims SET accepted_at_unix_ms=?2 WHERE id=?1",
                                        params![claim.id, (random % 1000).to_string()],
                                    )
                                    .unwrap();
                                }
                                _ => {
                                    tx.execute("DELETE FROM claims WHERE id=?1", [&claim.id])
                                        .unwrap();
                                    tx.execute("DELETE FROM local_observations WHERE id%2=0", [])
                                        .unwrap();
                                }
                            }
                            tx.commit().unwrap();
                        }
                    }
                }
                finish(&store);
                oracle(&store);
            }
        }
    }

    #[test]
    fn publication_facts_pending_loan_drains_only_one_page() {
        let store = Store::open_memory("node").unwrap();
        let mut connection = store.connection.write();
        let tx = connection.transaction().unwrap();
        for i in 0..192 {
            let claim = input("runtime.observed", &format!("pending-{i}"));
            append_claim_tx(
                &tx,
                "node",
                &claim.subject,
                &claim.kind,
                claim.actor.as_deref(),
                &json!({"fields": claim.fields}),
                &[],
                None,
            )
            .unwrap();
        }
        tx.commit().unwrap();
        drop(connection);
        let pending = || {
            store
                .readers
                .get()
                .query_row(
                    "SELECT COUNT(*) FROM local_publication_pending",
                    [],
                    |row| row.get::<_, usize>(0),
                )
                .unwrap()
        };
        let before = pending();
        assert!(before >= 128);
        assert!(!store.advance_publication_evidence().unwrap());
        let after = pending();
        assert!(after < before);
        assert!(before - after <= PAGE);
        finish(&store);
        oracle(&store);
    }
}
