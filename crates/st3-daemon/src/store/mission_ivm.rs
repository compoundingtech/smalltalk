//! Keyed mission selection over admitted operational projections.
//!
//! This shadow operator covers selection headers and run counts, not public card detail,
//! effective steps or actor authorization. The caller certifies projected-source coverage
//! before flushing; populated-source installation is explicit. Readers never seed or flush.
// Prepared shadow interface; production registration is owned by the source installer.
#![allow(dead_code)]
use super::*;
use smallclaims::ivm::{Definition, LocalChange, Readiness, View, Views, source_cut};

pub(crate) const VIEW: &str = "st3.mission-selection.v1";
const SOURCE: &str = "st3.mission-selection.changed";
const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS local_mission_counts (
 mission_id TEXT NOT NULL, status TEXT NOT NULL, count INTEGER NOT NULL CHECK(count>0),
 PRIMARY KEY(mission_id,status)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS local_mission_pending (mission_id TEXT PRIMARY KEY) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS local_mission_selection (
 mission_id TEXT PRIMARY KEY, updated_ms INTEGER NOT NULL, state TEXT NOT NULL,
 grace_until_ms INTEGER, visible INTEGER NOT NULL CHECK(visible IN (0,1)), body TEXT NOT NULL
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS local_mission_selection_order
 ON local_mission_selection(updated_ms DESC,mission_id ASC);
CREATE INDEX IF NOT EXISTS local_mission_selection_current_order
 ON local_mission_selection(updated_ms DESC,mission_id ASC) WHERE visible=1;
CREATE TABLE IF NOT EXISTS local_mission_clock (singleton INTEGER PRIMARY KEY CHECK(singleton=1), through_ms INTEGER NOT NULL);
INSERT OR IGNORE INTO local_mission_clock VALUES(1,0);
CREATE INDEX IF NOT EXISTS local_mission_selection_due
 ON local_mission_selection(grace_until_ms) WHERE visible=1 AND grace_until_ms IS NOT NULL;
CREATE INDEX IF NOT EXISTS local_mission_selection_deadline
 ON local_mission_selection(grace_until_ms) WHERE grace_until_ms IS NOT NULL AND mission_id NOT LIKE '__st3/%';
CREATE INDEX IF NOT EXISTS mission_ivm_latest_run
 ON mission_runs(mission_id,created_at_unix_ms DESC,id DESC);
CREATE TRIGGER IF NOT EXISTS mission_ivm_run_insert AFTER INSERT ON mission_runs BEGIN
 INSERT INTO local_mission_counts VALUES(NEW.mission_id,NEW.status,1)
 ON CONFLICT(mission_id,status) DO UPDATE SET count=count+1;
 INSERT INTO local_mission_pending VALUES(NEW.mission_id) ON CONFLICT(mission_id) DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS mission_ivm_run_delete AFTER DELETE ON mission_runs BEGIN
 DELETE FROM local_mission_counts WHERE mission_id=OLD.mission_id AND status=OLD.status AND count=1;
 UPDATE local_mission_counts SET count=count-1 WHERE mission_id=OLD.mission_id AND status=OLD.status;
 INSERT INTO local_mission_pending VALUES(OLD.mission_id) ON CONFLICT(mission_id) DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS mission_ivm_run_count_update
 AFTER UPDATE OF mission_id,status ON mission_runs
 WHEN NEW.mission_id<>OLD.mission_id OR NEW.status<>OLD.status BEGIN
 DELETE FROM local_mission_counts WHERE mission_id=OLD.mission_id AND status=OLD.status AND count=1;
 UPDATE local_mission_counts SET count=count-1 WHERE mission_id=OLD.mission_id AND status=OLD.status;
 INSERT INTO local_mission_counts VALUES(NEW.mission_id,NEW.status,1)
 ON CONFLICT(mission_id,status) DO UPDATE SET count=count+1;
END;
CREATE TRIGGER IF NOT EXISTS mission_ivm_run_update AFTER UPDATE ON mission_runs
 WHEN NEW.id<>OLD.id OR NEW.mission_id<>OLD.mission_id OR NEW.initial_revision<>OLD.initial_revision
 OR NEW.current_generation_id<>OLD.current_generation_id OR NEW.requester<>OLD.requester
 OR NEW.status<>OLD.status OR NEW.phase<>OLD.phase
 OR NEW.created_at_unix_ms<>OLD.created_at_unix_ms OR NEW.updated_at_unix_ms<>OLD.updated_at_unix_ms BEGIN
 INSERT INTO local_mission_pending VALUES(OLD.mission_id) ON CONFLICT(mission_id) DO NOTHING;
 INSERT INTO local_mission_pending VALUES(NEW.mission_id) ON CONFLICT(mission_id) DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS mission_ivm_definition_insert AFTER INSERT ON mission_definitions BEGIN
 INSERT INTO local_mission_pending VALUES(NEW.mission_id) ON CONFLICT(mission_id) DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS mission_ivm_definition_delete AFTER DELETE ON mission_definitions BEGIN
 INSERT INTO local_mission_pending VALUES(OLD.mission_id) ON CONFLICT(mission_id) DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS mission_ivm_definition_update AFTER UPDATE ON mission_definitions
 WHEN NEW.mission_id<>OLD.mission_id OR NEW.state<>OLD.state OR NEW.revision<>OLD.revision OR NEW.claim_id<>OLD.claim_id BEGIN
 INSERT INTO local_mission_pending VALUES(OLD.mission_id) ON CONFLICT(mission_id) DO NOTHING;
 INSERT INTO local_mission_pending VALUES(NEW.mission_id) ON CONFLICT(mission_id) DO NOTHING;
END;
-- Definition publication times are claim inputs. Updates/removals also fence generic IVM.
CREATE TRIGGER IF NOT EXISTS mission_ivm_publication_time AFTER UPDATE OF accepted_at_unix_ms ON claims
 WHEN NEW.accepted_at_unix_ms<>OLD.accepted_at_unix_ms BEGIN
 INSERT INTO local_mission_pending SELECT mission_id FROM mission_definitions WHERE claim_id=NEW.id ON CONFLICT(mission_id) DO NOTHING;
END;
"#;

pub(crate) fn definitions() -> Vec<Box<dyn View>> {
    vec![Box::new(MissionSelection)]
}

pub(crate) struct MissionSelection;
impl View for MissionSelection {
    fn definition(&self) -> Definition {
        Definition {
            name: VIEW,
            fingerprint: "selection.v1;projected-mission-definitions+run-headers;status-count-deltas;latest-created-text-desc-id-desc;updated-integer-desc-mission-asc;24h-failed-cancelled;captured-clock-current-membership;unicode-public-id-rust-lower;no-card-steps-auth",
            kinds: &[],
            local_kinds: &[SOURCE],
            max_contributions: 1,
        }
    }
    fn create_schema(&self, connection: &Connection) -> Result<()> {
        connection.execute_batch(SCHEMA)?;
        Ok(())
    }
    fn maintain_local_key(
        &self,
        tx: &Transaction<'_>,
        key: &str,
        change: &LocalChange,
    ) -> Result<bool> {
        maintain(tx, key, change.evaluation_time_unix_ms)
    }
}

fn maintain(tx: &Transaction<'_>, key: &str, at: u128) -> Result<bool> {
    let at = i64::try_from(at).context("mission source clock exceeds SQLite range")?;
    let mission = key
        .strip_prefix("mission/")
        .context("mission selection key")?;
    let definition: Option<(String, String, i64)> = tx
        .query_row(
            "SELECT d.state,d.revision,CAST(COALESCE(c.accepted_at_unix_ms,'0') AS INTEGER)
         FROM mission_definitions d LEFT JOIN claims c ON c.id=d.claim_id WHERE d.mission_id=?1",
            [mission],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let counts = tx
        .prepare_cached(
            "SELECT status,count FROM local_mission_counts WHERE mission_id=?1 ORDER BY status",
        )?
        .query_map([mission], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?))
        })?
        .collect::<rusqlite::Result<BTreeMap<_, _>>>()?;
    let latest: Option<(String, String, i64, String)> = tx.query_row(
        "SELECT id,status,CAST(updated_at_unix_ms AS INTEGER),
          json_object('id',id,'generation',current_generation_id,'revision',initial_revision,
           'requester',requester,'status',status,'phase',phase,'created',created_at_unix_ms,'updated',updated_at_unix_ms)
         FROM mission_runs WHERE mission_id=?1 ORDER BY created_at_unix_ms DESC,id DESC LIMIT 1",
        [mission], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?))).optional()?;
    let old: Option<String> = tx
        .query_row(
            "SELECT body FROM local_mission_selection WHERE mission_id=?1",
            [mission],
            |row| row.get(0),
        )
        .optional()?;
    if definition.is_none() && latest.is_none() {
        tx.execute(
            "DELETE FROM local_mission_selection WHERE mission_id=?1",
            [mission],
        )?;
        return Ok(old.is_some());
    }
    let running = counts.get("running").copied().unwrap_or(0);
    let standing = counts.get("standing").copied().unwrap_or(0);
    let retired = definition.as_ref().is_some_and(|d| d.0 == "retired");
    let state = if running > 0 {
        "running"
    } else if standing > 0 {
        "standing"
    } else if retired {
        "retired"
    } else if let Some(latest) = &latest {
        &latest.1
    } else {
        definition.as_ref().map(|d| d.0.as_str()).unwrap_or("ready")
    };
    let updated = latest
        .as_ref()
        .map(|r| r.2)
        .or_else(|| definition.as_ref().map(|d| d.2))
        .unwrap_or(0);
    let grace = latest
        .as_ref()
        .filter(|r| {
            matches!(r.1.as_str(), "failed" | "cancelled")
                && !retired
                && running == 0
                && standing == 0
        })
        .map(|r| {
            r.2.checked_add(24 * 60 * 60 * 1000)
                .context("mission deadline exceeds SQLite range")
        })
        .transpose()?;
    let external: bool =
        tx.query_row("SELECT ?1 NOT LIKE '__st3/%'", [mission], |row| row.get(0))?;
    let visible = external
        && (!matches!(state, "completed" | "failed" | "cancelled" | "retired")
            || grace.is_some_and(|until| until >= at));
    let latest_body = latest
        .as_ref()
        .map(|r| serde_json::from_str::<Value>(&r.3))
        .transpose()?;
    let body = canonical_json_text(
        &json!({"id":key,"search_id":key.to_lowercase(),"state":state,"updated_ms":updated,
        "definition":definition,"latest":latest_body,"counts":counts,"grace_until_ms":grace,"current":visible}),
    )?;
    if old.as_deref() == Some(&body) {
        return Ok(false);
    }
    tx.execute(
        "INSERT INTO local_mission_selection VALUES(?1,?2,?3,?4,?5,?6)
        ON CONFLICT(mission_id) DO UPDATE SET updated_ms=excluded.updated_ms,state=excluded.state,
          grace_until_ms=excluded.grace_until_ms,visible=excluded.visible,body=excluded.body",
        params![mission, updated, state, grace, visible, body],
    )?;
    Ok(true)
}

/// Drain at most `limit` affected missions. The owner supplies a source cut only after proving
/// complete projection/dependency coverage; changing the cut here cannot prove that coverage.
/// On a fenced view the queue remains intact for an explicitly initiated recovery installer.
pub(crate) fn flush(tx: &Transaction<'_>, views: &Views, at: u128, limit: usize) -> Result<usize> {
    anyhow::ensure!(
        (1..=1024).contains(&limit),
        "mission maintenance page bound"
    );
    let mut cut = source_cut(tx)?.context("mission selection source unavailable")?;
    anyhow::ensure!(
        cut.admitted == cut.projected && cut.admitted == current_index_tx(tx)?,
        "mission selection source pending"
    );
    anyhow::ensure!(
        matches!(views.readiness(tx, VIEW, cut.epoch)?, Readiness::Ready(_)),
        "mission selection unavailable"
    );
    let clock: i64 = tx.query_row(
        "SELECT through_ms FROM local_mission_clock WHERE singleton=1",
        [],
        |row| row.get(0),
    )?;
    anyhow::ensure!(
        i64::try_from(at)? >= clock,
        "mission source clock moved backwards"
    );
    let keys = tx
        .prepare_cached(
            "SELECT mission_id FROM local_mission_pending ORDER BY mission_id LIMIT ?1",
        )?
        .query_map([limit], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for mission in &keys {
        cut.local_generation = cut
            .local_generation
            .checked_add(1)
            .context("local generation exhausted")?;
        let changed = views.local_change(
            tx,
            &LocalChange {
                kind: SOURCE.into(),
                old_keys: BTreeSet::new(),
                new_keys: BTreeSet::from([format!("mission/{mission}")]),
                evaluation_time_unix_ms: at,
            },
            cut,
        )?;
        anyhow::ensure!(
            changed.deferred.is_empty(),
            "mission selection maintenance deferred"
        );
        tx.execute(
            "DELETE FROM local_mission_pending WHERE mission_id=?1",
            [mission],
        )?;
    }
    tx.execute(
        "UPDATE local_mission_clock SET through_ms=?1 WHERE through_ms<?1",
        [i64::try_from(at)?],
    )?;
    Ok(keys.len())
}

/// Explicit captured-time maintenance. Due candidates use a partial deadline index and
/// each page changes at most `limit` public keys. Reads refuse overdue membership.
pub(crate) fn clock_page(
    tx: &Transaction<'_>,
    views: &Views,
    at: u128,
    limit: usize,
) -> Result<usize> {
    anyhow::ensure!((1..=1024).contains(&limit), "mission clock page bound");
    ready(tx, views)?;
    let at_ms = i64::try_from(at)?;
    let previous: i64 = tx.query_row(
        "SELECT through_ms FROM local_mission_clock WHERE singleton=1",
        [],
        |row| row.get(0),
    )?;
    anyhow::ensure!(at_ms >= previous, "mission clock moved backwards");
    let keys = tx
        .prepare_cached(
            "SELECT mission_id FROM local_mission_selection
        WHERE visible=1 AND grace_until_ms IS NOT NULL AND grace_until_ms<?1
        ORDER BY grace_until_ms LIMIT ?2",
        )?
        .query_map(params![at_ms, limit], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut cut = source_cut(tx)?.context("mission clock source unavailable")?;
    for id in &keys {
        cut.local_generation = cut
            .local_generation
            .checked_add(1)
            .context("local generation exhausted")?;
        let changes = views.local_change(
            tx,
            &LocalChange {
                kind: SOURCE.into(),
                old_keys: BTreeSet::new(),
                new_keys: BTreeSet::from([format!("mission/{id}")]),
                evaluation_time_unix_ms: at,
            },
            cut,
        )?;
        anyhow::ensure!(
            changes.deferred.is_empty(),
            "mission clock maintenance deferred"
        );
    }
    tx.execute(
        "UPDATE local_mission_clock SET through_ms=?1 WHERE singleton=1 AND through_ms<?1",
        [at_ms],
    )?;
    Ok(keys.len())
}

fn ready_at(connection: &Connection, views: &Views, at: u128) -> Result<()> {
    ready(connection, views)?;
    let at = i64::try_from(at)?;
    let previous: i64 = connection.query_row(
        "SELECT through_ms FROM local_mission_clock WHERE singleton=1",
        [],
        |row| row.get(0),
    )?;
    anyhow::ensure!(
        at >= previous,
        "mission clock snapshot predates maintained membership"
    );
    let due: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM local_mission_selection
        WHERE visible=1 AND grace_until_ms IS NOT NULL AND grace_until_ms<?1)",
        [at],
        |row| row.get(0),
    )?;
    anyhow::ensure!(!due, "mission clock membership pending");
    Ok(())
}

fn ready(connection: &Connection, views: &Views) -> Result<()> {
    let cut = source_cut(connection)?.context("mission selection source unavailable")?;
    anyhow::ensure!(
        matches!(
            views.readiness(connection, VIEW, cut.epoch)?,
            Readiness::Ready(_)
        ),
        "mission selection unavailable"
    );
    let dirty: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM local_mission_pending)",
        [],
        |row| row.get(0),
    )?;
    anyhow::ensure!(!dirty, "mission selection has unflushed source changes");
    Ok(())
}

/// Uses the existing timestamp DESC / public ID ASC continuation tuple. Membership is
/// evaluated at the caller's captured time, including the inclusive end of the grace period.
pub(crate) fn window(
    connection: &Connection,
    views: &Views,
    history: bool,
    at: u128,
    after: Option<&(u128, String)>,
    text: Option<&str>,
    limit: usize,
) -> Result<Vec<(String, u128)>> {
    anyhow::ensure!((1..=501).contains(&limit), "mission window page bound");
    let at = i64::try_from(at).context("mission snapshot time exceeds SQLite range")?;
    let after_ms = after
        .map(|key| i64::try_from(key.0))
        .transpose()
        .context("mission seek time exceeds SQLite range")?;
    ready_at(connection, views, at as u128)?;
    let membership = if history { "1" } else { "visible=1" };
    // IDs are Unicode-lowercased on maintenance, matching the public list matcher. Only
    // the bounded needle is normalized here; SQLite lower() covers ASCII alone.
    let text = text.map(str::to_lowercase);
    let seek = if after.is_some() {
        "updated_ms<=?3 AND (updated_ms<?3 OR mission_id>?4)"
    } else {
        "?3 IS NULL AND ?4 IS NULL"
    };
    let sql = format!(
        "SELECT mission_id,updated_ms FROM local_mission_selection
        WHERE {membership} AND ?1 IN (0,1) AND ?2>=0 AND ({seek})
          AND (?6 IS NULL OR instr(json_extract(body,'$.search_id'),?6)>0)
        ORDER BY updated_ms DESC,mission_id ASC LIMIT ?5"
    );
    connection
        .prepare_cached(&sql)?
        .query_map(
            params![
                history,
                at,
                after_ms,
                after.map(|key| key.1.trim_start_matches("mission/")),
                limit,
                text
            ],
            |row| {
                Ok((
                    format!("mission/{}", row.get::<_, String>(0)?),
                    row.get::<_, u64>(1)? as u128,
                ))
            },
        )?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

pub(crate) fn row(
    connection: &Connection,
    views: &Views,
    at: u128,
    key: &str,
) -> Result<Option<Value>> {
    ready_at(connection, views, at)?;
    connection
        .query_row(
            "SELECT body FROM local_mission_selection WHERE mission_id=?1",
            [key.trim_start_matches("mission/")],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .map(|body| serde_json::from_str(&body))
        .transpose()
        .map_err(Into::into)
}

/// One indexed query for the already selected keys; callers retain their window order.
pub(crate) fn rows(
    connection: &Connection,
    views: &Views,
    at: u128,
    keys: &[String],
) -> Result<BTreeMap<String, Value>> {
    anyhow::ensure!(keys.len() <= 501, "mission selected-key batch bound");
    ready_at(connection, views, at)?;
    let ids = keys
        .iter()
        .map(|key| key.trim_start_matches("mission/"))
        .collect::<Vec<_>>();
    let bodies = connection.prepare_cached(
        "SELECT mission_id,body FROM local_mission_selection WHERE mission_id IN (SELECT value FROM json_each(?1))"
    )?.query_map([serde_json::to_string(&ids)?], |row| Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?)))?
      .collect::<rusqlite::Result<Vec<_>>>()?;
    bodies
        .into_iter()
        .map(|(id, body)| Ok((format!("mission/{id}"), serde_json::from_str(&body)?)))
        .collect()
}

pub(crate) fn clean(connection: &Connection) -> Result<bool> {
    connection
        .query_row(
            "SELECT NOT EXISTS(SELECT 1 FROM local_mission_pending)",
            [],
            |row| row.get(0),
        )
        .map_err(Into::into)
}

/// Earliest captured-time membership change for transport/reconciler timer ownership.
pub(crate) fn next_deadline(
    connection: &Connection,
    views: &Views,
    at: u128,
) -> Result<Option<u128>> {
    ready_at(connection, views, at)?;
    let time: Option<u64> = connection
        .query_row(
            "SELECT grace_until_ms+1 FROM local_mission_selection
      WHERE grace_until_ms IS NOT NULL AND mission_id NOT LIKE '__st3/%' AND grace_until_ms>=?1 ORDER BY grace_until_ms LIMIT 1",
            [i64::try_from(at)?],
            |row| row.get(0),
        )
        .optional()?
        .flatten();
    Ok(time.map(u128::from))
}

#[cfg(test)]
mod tests {
    use super::super::tests::{exchange_from, receive_and_project};
    use super::*;
    use smallclaims::ivm::{SourceCut, events};

    fn register(store: &Store) -> Arc<Views> {
        let views = Arc::new(Views::new(definitions()).unwrap());
        let mut writer = store.connection.write();
        let tx = writer.transaction().unwrap();
        assert_eq!(
            current_index_tx(&tx).unwrap(),
            0,
            "fixture registration is empty-source only"
        );
        views.create_schema(&tx).unwrap();
        views
            .initialize_empty(
                &tx,
                SourceCut {
                    epoch: 1,
                    admitted: 0,
                    projected: 0,
                    local_generation: 0,
                },
            )
            .unwrap();
        events::install(&tx, 1024).unwrap();
        tx.commit().unwrap();
        views
    }
    fn fixture() -> (Store, Arc<Views>) {
        let store = Store::open_memory("birch").unwrap();
        let views = register(&store);
        (store, views)
    }
    // Test-only coverage certificate: every real Store action above has returned, or a real
    // replication projection pass has completed. This is not a production MAX-index adapter.
    fn checkpoint(store: &Store, views: &Views, at: u128) {
        let mut writer = store.connection.write();
        let tx = writer.transaction().unwrap();
        let index = current_index_tx(&tx).unwrap();
        let previous = source_cut(&tx).unwrap().unwrap();
        views
            .publish_cut(
                &tx,
                SourceCut {
                    admitted: index,
                    projected: index,
                    ..previous
                },
            )
            .unwrap();
        while flush(&tx, views, at, 2).unwrap() != 0 {}
        while clock_page(&tx, views, at, 2).unwrap() != 0 {}
        tx.commit().unwrap();
    }
    fn publish(store: &Store, id: &str, key: &str) {
        let source = format!(
            "version 2\nmission {id:?} state=\"ready\" {{ concurrent-runs max=4; goal \"Prepare a sample.\"; step \"build\" {{ goal \"Build a sample.\"; }} }}\n"
        );
        let intent = crate::parse_intent(&source, store.origin()).unwrap();
        store.apply_internal(&intent, key).unwrap();
    }
    fn start(store: &Store, id: &str, key: &str) -> MissionRunView {
        store
            .create_mission_run(&MissionRunRequest {
                mission: id.into(),
                revision: None,
                workspace: "/example/project".into(),
                requester: Some("person/avery".into()),
                mode: None,
                inputs: BTreeMap::new(),
                idempotency_key: key.into(),
            })
            .unwrap()
    }
    fn parity(store: &Store, views: &Views, at: u128) {
        checkpoint(store, views, at);
        store.read_snapshot(|_| {
            let connection=store.readers.get();
            for history in [false,true] {
                let expected=store.mission_collection_page(history,0,501,None)?;
                assert_eq!(window(&connection,views,history,at,None,None,501)?,expected);
                let mut paged=Vec::new();
                let mut after=None;
                loop {
                    let page=window(&connection,views,history,at,after.as_ref(),None,2)?;
                    if page.is_empty(){break;}
                    let last=page.last().unwrap();
                    after=Some((last.1,last.0.clone()));
                    paged.extend(page);
                }
                assert_eq!(paged,expected,"existing full-sort seek tuple");
            }
            // Independent SQL count oracle, not the incremental counters.
            let counts=connection.prepare("SELECT mission_id,status,COUNT(*) FROM mission_runs GROUP BY mission_id,status ORDER BY mission_id,status")?
                .query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,u64>(2)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let actual=connection.prepare("SELECT mission_id,status,count FROM local_mission_counts ORDER BY mission_id,status")?
                .query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,u64>(2)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            assert_eq!(actual,counts);
            Ok(())
        }).unwrap();
    }

    #[test]
    fn real_store_selection_parity_and_bounded_seeks() {
        let _clock = clock_snapshot();
        let at = now_ms();
        let (store, views) = fixture();
        for id in ["orchard", "harbor", "summit", "__st3/internal"] {
            publish(&store, id, &format!("publish-{id}"));
            parity(&store, &views, at);
            if id.starts_with("__st3/") {
                continue;
            }
            for n in 0..4 {
                start(&store, id, &format!("start-{id}-{n}"));
            }
            parity(&store, &views, at);
        }
        let connection = store.readers.get();
        let row = row(&connection, &views, at, "mission/orchard")
            .unwrap()
            .unwrap();
        assert_eq!(row["counts"]["running"], 4);
        assert_eq!(row["state"], "running");
        assert!(window(&connection, &views, true, at, None, None, 502).is_err());
    }

    #[test]
    fn changes_are_pending_until_flushed_and_rollback_is_atomic() {
        let _clock = clock_snapshot();
        let at = now_ms();
        let (store, views) = fixture();
        publish(&store, "orchard", "publish");
        assert!(window(&store.readers.get(), &views, false, at, None, None, 3).is_err());
        parity(&store, &views, at);
        let run = start(&store, "orchard", "start");
        parity(&store, &views, at);
        let before = row(&store.readers.get(), &views, at, "mission/orchard").unwrap();
        let token = views.readiness(&store.readers.get(), VIEW, 1).unwrap();
        {
            let mut writer = store.connection.write();
            let tx = writer.transaction().unwrap();
            tx.execute(
                "UPDATE mission_runs SET status='failed',phase='terminal' WHERE id=?1",
                [&run.id],
            )
            .unwrap();
            assert!(
                row(&tx, &views, at, "mission/orchard").is_err(),
                "unflushed projected source"
            );
            flush(&tx, &views, at, 1).unwrap();
            assert_ne!(row(&tx, &views, at, "mission/orchard").unwrap(), before);
            // Drop the source+counter+output+generation transaction without committing.
        }
        assert_eq!(
            row(&store.readers.get(), &views, at, "mission/orchard").unwrap(),
            before
        );
        assert_eq!(
            views.readiness(&store.readers.get(), VIEW, 1).unwrap(),
            token
        );
        parity(&store, &views, at);
    }

    #[test]
    fn old_new_membership_and_definition_removal_preserve_counts() {
        let _clock = clock_snapshot();
        let at = now_ms();
        let (store, views) = fixture();
        publish(&store, "orchard", "publish");
        let run = start(&store, "orchard", "start");
        parity(&store, &views, at);
        {
            let writer = store.connection.write();
            writer
                .execute(
                    "UPDATE mission_runs SET mission_id='harbor',status='standing' WHERE id=?1",
                    [&run.id],
                )
                .unwrap();
        }
        parity(&store, &views, at);
        let old = row(&store.readers.get(), &views, at, "mission/orchard")
            .unwrap()
            .unwrap();
        assert_eq!(old["counts"], json!({}));
        let new = row(&store.readers.get(), &views, at, "mission/harbor")
            .unwrap()
            .unwrap();
        assert_eq!(new["counts"], json!({"standing":1}));
        {
            let writer = store.connection.write();
            writer
                .execute(
                    "DELETE FROM mission_definitions WHERE mission_id='orchard'",
                    [],
                )
                .unwrap();
        }
        parity(&store, &views, at);
        assert!(
            row(&store.readers.get(), &views, at, "mission/orchard")
                .unwrap()
                .is_none()
        );
        let changed = views
            .changed_keys(&store.readers.get(), VIEW, 1, 0, 100, None)
            .unwrap();
        assert!(
            changed.keys.iter().any(|key| key.key == "mission/orchard"),
            "removal retained in key feed"
        );
    }

    #[test]
    fn unrelated_writes_and_identical_run_updates_touch_no_output() {
        let _clock = clock_snapshot();
        let at = now_ms();
        let (store, views) = fixture();
        publish(&store, "orchard", "publish");
        let run = start(&store, "orchard", "start");
        parity(&store, &views, at);
        let token = views.readiness(&store.readers.get(), VIEW, 1).unwrap();
        {
            let writer = store.connection.write();
            writer.execute_batch("CREATE TEMP TABLE mission_mutations(kind TEXT);
                CREATE TEMP TRIGGER mission_output_updated AFTER UPDATE ON local_mission_selection BEGIN INSERT INTO mission_mutations VALUES('update'); END;
                CREATE TEMP TRIGGER mission_output_inserted AFTER INSERT ON local_mission_selection BEGIN INSERT INTO mission_mutations VALUES('insert'); END;
                CREATE TEMP TRIGGER mission_output_deleted AFTER DELETE ON local_mission_selection BEGIN INSERT INTO mission_mutations VALUES('delete'); END;").unwrap();
            writer.execute("UPDATE mission_runs SET status=status,updated_at_unix_ms=updated_at_unix_ms WHERE id=?1",[&run.id]).unwrap();
        }
        store
            .append_claim(&ClaimInput {
                subject: "resource/sample".into(),
                kind: "resource.observed".into(),
                actor: None,
                fields: BTreeMap::from([("kind".into(), json!("custom.example.sample"))]),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        parity(&store, &views, at);
        assert_eq!(
            views.readiness(&store.readers.get(), VIEW, 1).unwrap(),
            token
        );
        let count: u64 = store
            .connection
            .write()
            .query_row("SELECT COUNT(*) FROM mission_mutations", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 0, "unchanged tokens cannot conceal row rewrites");
    }

    #[test]
    fn replicated_permutations_and_duplicates_match_real_store_oracle() {
        let _clock = clock_snapshot();
        let at = now_ms();
        let source = Store::open_memory("cedar").unwrap();
        for id in ["orchard", "harbor", "summit"] {
            publish(&source, id, &format!("publish-{id}"));
            start(&source, id, &format!("start-{id}"));
        }
        let exchange = exchange_from(&source, &ReplicationInventory::default());
        assert!(
            exchange.envelopes.len() >= 3,
            "meaningful envelope permutation"
        );
        for reverse in [false, true] {
            let (target, views) = fixture();
            let mut permuted = exchange.clone();
            if reverse {
                permuted.envelopes.reverse();
            }
            receive_and_project(&target, "cedar", &permuted);
            parity(&target, &views, at);
            receive_and_project(&target, "cedar", &permuted);
            parity(&target, &views, at);
            let expected = source.mission_collection_page(true, 0, 501, None).unwrap();
            assert_eq!(
                window(&target.readers.get(), &views, true, at, None, None, 501).unwrap(),
                expected
            );
        }
    }
    #[test]
    fn captured_time_grace_deadlines_and_retirement_match_the_store() {
        struct Clock;
        impl Drop for Clock {
            fn drop(&mut self) {
                smallclaims::store::set_thread_clock(None);
            }
        }
        let _reset = Clock;
        let at = 1_800_000_000_000u128;
        smallclaims::store::set_thread_clock(Some(at));
        let (store, views) = fixture();
        publish(&store, "orchard", "publish");
        let run = start(&store, "orchard", "start");
        store
            .set_mission_run_state(&run.id, "cancelled", "terminal", Some("sample ended"))
            .unwrap();
        parity(&store, &views, at);
        let deadline = at + RECENTLY_ENDED_MS;
        for moment in [deadline - 1, deadline, deadline + 1] {
            smallclaims::store::set_thread_clock(Some(moment));
            parity(&store, &views, moment);
            let ids = window(&store.readers.get(), &views, false, moment, None, None, 5).unwrap();
            assert_eq!(!ids.is_empty(), moment <= deadline);
            assert_eq!(
                next_deadline(&store.readers.get(), &views, moment).unwrap(),
                (moment <= deadline).then_some(deadline + 1)
            );
        }
        smallclaims::store::set_thread_clock(Some(deadline + 1));
        store
            .retire_mission("mission/orchard", "person/avery", "retire")
            .unwrap();
        parity(&store, &views, deadline + 1);
        assert!(
            window(
                &store.readers.get(),
                &views,
                false,
                deadline + 1,
                None,
                None,
                5
            )
            .unwrap()
            .is_empty()
        );
        assert_eq!(
            next_deadline(&store.readers.get(), &views, deadline + 1).unwrap(),
            None
        );
    }

    #[test]
    fn filters_apply_before_slots_and_selected_headers_are_batched() {
        let _clock = clock_snapshot();
        let at = now_ms();
        let (store, views) = fixture();
        for (n, id) in [
            "alpha",
            "birch",
            "cedar",
            "grove-100%",
            "grove_under",
            "Äpfel",
            "Σύνοδος",
        ]
        .iter()
        .enumerate()
        {
            publish(&store, id, &format!("publish-{n}"));
        }
        parity(&store, &views, at);
        let connection = store.readers.get();
        for filter in [
            "grove",
            "%",
            "_",
            "mission/grove",
            "absent",
            "ä",
            "Ä",
            "ÄPFEL",
            "ΣΎ",
        ] {
            let all = window(&connection, &views, true, at, None, None, 501).unwrap();
            let expected = all
                .into_iter()
                .filter(|(id, _)| id.to_lowercase().contains(&filter.to_lowercase()))
                .take(1)
                .collect::<Vec<_>>();
            assert_eq!(
                window(&connection, &views, true, at, None, Some(filter), 1).unwrap(),
                expected
            );
        }
        let ids = window(&connection, &views, true, at, None, None, 501)
            .unwrap()
            .into_iter()
            .map(|(id, _)| id)
            .collect::<Vec<_>>();
        let selected = rows(&connection, &views, at, &ids).unwrap();
        assert_eq!(selected.len(), 7);
        for id in ids {
            assert_eq!(
                selected.get(&id),
                row(&connection, &views, at, &id).unwrap().as_ref()
            );
        }
    }

    #[test]
    fn reopen_preserves_rows_and_source_mutations_fence_instead_of_replaying() {
        let _clock = clock_snapshot();
        let at = now_ms();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sample.sqlite3");
        let store = Store::open(&path, "birch").unwrap();
        let views = register(&store);
        publish(&store, "orchard", "publish");
        start(&store, "orchard", "start");
        parity(&store, &views, at);
        let before = row(&store.readers.get(), &views, at, "mission/orchard").unwrap();
        let token = views.readiness(&store.readers.get(), VIEW, 1).unwrap();
        drop(store);
        let reopened = Store::open(&path, "birch").unwrap();
        assert_eq!(
            row(&reopened.readers.get(), &views, at, "mission/orchard").unwrap(),
            before
        );
        assert_eq!(
            views.readiness(&reopened.readers.get(), VIEW, 1).unwrap(),
            token
        );
        let claim = reopened
            .latest_claim("mission/orchard", Some("mission.published"))
            .unwrap()
            .unwrap();
        reopened
            .connection
            .write()
            .execute(
                "UPDATE claims SET accepted_at_unix_ms='1' WHERE id=?1",
                [claim.id],
            )
            .unwrap();
        assert_eq!(
            views.readiness(&reopened.readers.get(), VIEW, 1).unwrap(),
            Readiness::Fenced
        );
        assert!(row(&reopened.readers.get(), &views, at, "mission/orchard").is_err());
        let mut writer = reopened.connection.write();
        let tx = writer.transaction().unwrap();
        assert!(flush(&tx, &views, at, 128).is_err());
        assert!(!clean(&tx).unwrap(), "fenced source retains recovery work");
    }
    #[test]
    fn clock_pages_refuse_partial_membership_and_use_indexed_candidates() {
        let _clock = clock_snapshot();
        let at = now_ms();
        let (store, views) = fixture();
        for id in ["orchard", "harbor", "summit"] {
            publish(&store, id, &format!("publish-{id}"));
            let run = start(&store, id, &format!("start-{id}"));
            store
                .set_mission_run_state(&run.id, "cancelled", "terminal", None)
                .unwrap();
        }
        parity(&store, &views, at);
        let future: u128 = store
            .readers
            .get()
            .query_row(
                "SELECT MAX(grace_until_ms)+1 FROM local_mission_selection",
                [],
                |row| row.get::<_, u64>(0),
            )
            .unwrap()
            .into();
        assert!(
            window(&store.readers.get(), &views, false, future, None, None, 5).is_err(),
            "read never repairs overdue membership"
        );
        let mut writer = store.connection.write();
        let tx = writer.transaction().unwrap();
        assert_eq!(clock_page(&tx, &views, future, 1).unwrap(), 1);
        assert!(
            window(&tx, &views, false, future, None, None, 5).is_err(),
            "partial time page remains unavailable"
        );
        assert_eq!(clock_page(&tx, &views, future, 1).unwrap(), 1);
        assert_eq!(clock_page(&tx, &views, future, 1).unwrap(), 1);
        assert_eq!(clock_page(&tx, &views, future, 1).unwrap(), 0);
        assert!(
            window(&tx, &views, false, future, None, None, 5)
                .unwrap()
                .is_empty()
        );
        assert!(
            window(&tx, &views, false, at, None, None, 5).is_err(),
            "captured time cannot move backwards"
        );
        for (sql, index) in [
            (
                "SELECT mission_id FROM local_mission_selection WHERE visible=1 AND updated_ms<=1 AND (updated_ms<1 OR mission_id>'sample') ORDER BY updated_ms DESC,mission_id ASC LIMIT 5",
                "local_mission_selection_current_order",
            ),
            (
                "SELECT mission_id FROM local_mission_selection WHERE visible=1 AND grace_until_ms IS NOT NULL AND grace_until_ms<1 ORDER BY grace_until_ms LIMIT 5",
                "local_mission_selection_due",
            ),
            (
                "SELECT id FROM mission_runs WHERE mission_id='orchard' ORDER BY created_at_unix_ms DESC,id DESC LIMIT 1",
                "mission_ivm_latest_run",
            ),
        ] {
            let plan = tx
                .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
                .unwrap()
                .query_map([], |row| row.get::<_, String>(3))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
                .join("\n");
            assert!(plan.contains(index), "{plan}");
            assert!(!plan.contains("TEMP B-TREE"), "{plan}");
        }
        tx.commit().unwrap();
    }
}
