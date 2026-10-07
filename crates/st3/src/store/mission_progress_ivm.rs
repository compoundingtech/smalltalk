//! Prepared current-generation raw progress counts, not effective steps or full cards.
//! Source capture is indexed and independent of arrival order. Installation is explicit;
//! the shared source owner certifies the cut and publishes readiness after bounded backfill.
#![allow(dead_code)]

#[cfg(test)]
#[path = "mission_progress_ivm_tests.rs"]
mod tests;

use super::*;
use smallclaims::ivm::{Definition, LocalChange, Readiness, View, Views, source_cut};

pub(crate) const VIEW: &str = "st3.mission-progress-counts.v1";
const SOURCE: &str = "st3.mission-progress-counts.changed";
const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS local_progress_sources (
 subject TEXT PRIMARY KEY, run_id TEXT NOT NULL, generation_id TEXT NOT NULL, status TEXT NOT NULL CHECK(status IN ('completed','other'))
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS local_progress_counts (
 run_id TEXT NOT NULL, generation_id TEXT NOT NULL, status TEXT NOT NULL,
 count INTEGER NOT NULL CHECK(count>0), PRIMARY KEY(run_id,generation_id,status)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS local_progress_pending (run_id TEXT PRIMARY KEY) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS local_progress_rows (
 run_id TEXT PRIMARY KEY, generation_id TEXT NOT NULL, total INTEGER NOT NULL, done INTEGER NOT NULL
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS local_progress_capture (
 singleton INTEGER PRIMARY KEY CHECK(singleton=1), complete INTEGER NOT NULL CHECK(complete IN (0,1)),
 cursor TEXT
);
-- Inventory capture starts empty even for populated sources. The durable explicit cursor
-- prevents caller-provided seeks from skipping uncaptured rows. Subsequent source mutations
-- are captured by triggers, including inserts before an in-progress cursor.
INSERT OR IGNORE INTO local_progress_capture
 SELECT 1,NOT EXISTS(SELECT 1 FROM step_runs),NULL;
CREATE TABLE IF NOT EXISTS local_progress_run_capture (
 singleton INTEGER PRIMARY KEY CHECK(singleton=1), complete INTEGER NOT NULL CHECK(complete IN (0,1)), cursor TEXT
);
INSERT OR IGNORE INTO local_progress_run_capture SELECT 1,NOT EXISTS(SELECT 1 FROM mission_runs),NULL;

CREATE TRIGGER IF NOT EXISTS progress_inventory_insert AFTER INSERT ON local_progress_sources BEGIN
 INSERT INTO local_progress_counts VALUES(NEW.run_id,NEW.generation_id,NEW.status,1)
 ON CONFLICT(run_id,generation_id,status) DO UPDATE SET count=count+1;
 INSERT INTO local_progress_pending VALUES(NEW.run_id) ON CONFLICT DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS progress_inventory_delete AFTER DELETE ON local_progress_sources BEGIN
 DELETE FROM local_progress_counts WHERE run_id=OLD.run_id AND generation_id=OLD.generation_id AND status=OLD.status AND count=1;
 UPDATE local_progress_counts SET count=count-1 WHERE run_id=OLD.run_id AND generation_id=OLD.generation_id AND status=OLD.status;
 INSERT INTO local_progress_pending VALUES(OLD.run_id) ON CONFLICT DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS progress_inventory_update AFTER UPDATE ON local_progress_sources
 WHEN NEW.run_id<>OLD.run_id OR NEW.generation_id<>OLD.generation_id OR NEW.status<>OLD.status BEGIN
 DELETE FROM local_progress_counts WHERE run_id=OLD.run_id AND generation_id=OLD.generation_id AND status=OLD.status AND count=1;
 UPDATE local_progress_counts SET count=count-1 WHERE run_id=OLD.run_id AND generation_id=OLD.generation_id AND status=OLD.status;
 INSERT INTO local_progress_counts VALUES(NEW.run_id,NEW.generation_id,NEW.status,1)
 ON CONFLICT(run_id,generation_id,status) DO UPDATE SET count=count+1;
 INSERT INTO local_progress_pending VALUES(OLD.run_id) ON CONFLICT DO NOTHING;
 INSERT INTO local_progress_pending VALUES(NEW.run_id) ON CONFLICT DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS progress_step_insert AFTER INSERT ON step_runs BEGIN
 INSERT INTO local_progress_sources VALUES(NEW.subject,NEW.run_id,NEW.generation_id,CASE WHEN NEW.status='completed' THEN 'completed' ELSE 'other' END)
 ON CONFLICT(subject) DO UPDATE SET run_id=excluded.run_id,generation_id=excluded.generation_id,status=excluded.status
 WHERE run_id<>excluded.run_id OR generation_id<>excluded.generation_id OR status<>excluded.status;
END;
CREATE TRIGGER IF NOT EXISTS progress_step_delete AFTER DELETE ON step_runs BEGIN
 DELETE FROM local_progress_sources WHERE subject=OLD.subject;
END;
CREATE TRIGGER IF NOT EXISTS progress_step_update
 AFTER UPDATE OF subject,run_id,generation_id,status ON step_runs
 WHEN NEW.subject<>OLD.subject OR NEW.run_id<>OLD.run_id OR NEW.generation_id<>OLD.generation_id OR NEW.status<>OLD.status BEGIN
 DELETE FROM local_progress_sources WHERE subject=OLD.subject AND OLD.subject<>NEW.subject;
 INSERT INTO local_progress_sources VALUES(NEW.subject,NEW.run_id,NEW.generation_id,CASE WHEN NEW.status='completed' THEN 'completed' ELSE 'other' END)
 ON CONFLICT(subject) DO UPDATE SET run_id=excluded.run_id,generation_id=excluded.generation_id,status=excluded.status
 WHERE run_id<>excluded.run_id OR generation_id<>excluded.generation_id OR status<>excluded.status;
END;
CREATE TRIGGER IF NOT EXISTS progress_run_insert AFTER INSERT ON mission_runs BEGIN
 INSERT INTO local_progress_pending VALUES(NEW.id) ON CONFLICT DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS progress_run_delete AFTER DELETE ON mission_runs BEGIN
 INSERT INTO local_progress_pending VALUES(OLD.id) ON CONFLICT DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS progress_generation_update AFTER UPDATE OF id,current_generation_id ON mission_runs
 WHEN NEW.id<>OLD.id OR NEW.current_generation_id<>OLD.current_generation_id BEGIN
 INSERT INTO local_progress_pending VALUES(OLD.id) ON CONFLICT DO NOTHING;
 INSERT INTO local_progress_pending VALUES(NEW.id) ON CONFLICT DO NOTHING;
END;
"#;
const UPSERT_SOURCE: &str = "INSERT INTO local_progress_sources VALUES(?1,?2,?3,?4)
 ON CONFLICT(subject) DO UPDATE SET run_id=excluded.run_id,generation_id=excluded.generation_id,status=excluded.status
 WHERE run_id<>excluded.run_id OR generation_id<>excluded.generation_id OR status<>excluded.status";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Progress {
    pub generation: String,
    pub total: u64,
    pub done: u64,
}
pub(crate) fn definitions() -> Vec<Box<dyn View>> {
    vec![Box::new(Counts)]
}
struct Counts;
impl View for Counts {
    fn definition(&self) -> Definition {
        Definition {
            name: VIEW,
            fingerprint: "progress-counts.v1;inventory-capture.v1;old-new-run-generation-binary-completion;current-generation;raw-total-completed;no-effective-step-or-card-authority",
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
        _change: &LocalChange,
    ) -> Result<bool> {
        maintain(tx, key)
    }
}
fn maintain(tx: &Transaction<'_>, key: &str) -> Result<bool> {
    let run = key
        .strip_prefix("mission-run/")
        .context("progress run key")?;
    let generation: Option<String> = tx
        .query_row(
            "SELECT current_generation_id FROM mission_runs WHERE id=?1",
            [run],
            |row| row.get(0),
        )
        .optional()?;
    let old: Option<(String, u64, u64)> = tx
        .query_row(
            "SELECT generation_id,total,done FROM local_progress_rows WHERE run_id=?1",
            [run],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some(generation) = generation else {
        tx.execute("DELETE FROM local_progress_rows WHERE run_id=?1", [run])?;
        return Ok(old.is_some());
    };
    let mut total = 0u64;
    let mut done = 0u64;
    let counts = tx
        .prepare_cached(
            "SELECT status,count FROM local_progress_counts WHERE run_id=?1 AND generation_id=?2",
        )?
        .query_map(params![run, generation], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    // Exactly two completion buckets preserve raw COUNT/SUM even for an unknown status.
    for (status, count) in counts {
        total = total
            .checked_add(count)
            .context("progress count overflow")?;
        if status == "completed" {
            done = count;
        }
    }
    if old.as_ref() == Some(&(generation.clone(), total, done)) {
        return Ok(false);
    }
    tx.execute("INSERT INTO local_progress_rows VALUES(?1,?2,?3,?4) ON CONFLICT(run_id) DO UPDATE SET generation_id=excluded.generation_id,total=excluded.total,done=excluded.done",params![run,generation,total,done])?;
    Ok(true)
}

/// Explicit resumable inventory capture. Changes while capture is incomplete are already
/// tracked by source triggers. This never sets the primitive view Ready or certifies claims.
pub(crate) fn seed_page(tx: &Transaction<'_>, limit: usize) -> Result<(usize, bool)> {
    anyhow::ensure!((1..=1024).contains(&limit), "progress seed page bound");
    let (mut complete, cursor): (bool, Option<String>) = tx.query_row(
        "SELECT complete,cursor FROM local_progress_capture WHERE singleton=1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let (mut runs_complete, run_cursor): (bool, Option<String>) = tx.query_row(
        "SELECT complete,cursor FROM local_progress_run_capture WHERE singleton=1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if complete && runs_complete {
        return Ok((0, true));
    }
    require_fenced(tx)?;
    let mut processed = 0;
    if !complete {
        let sql = if cursor.is_some() {
            "SELECT subject,run_id,generation_id,CASE WHEN status='completed' THEN 'completed' ELSE 'other' END FROM step_runs WHERE subject>?1 ORDER BY subject LIMIT ?2"
        } else {
            "SELECT subject,run_id,generation_id,CASE WHEN status='completed' THEN 'completed' ELSE 'other' END FROM step_runs WHERE ?1 IS NULL ORDER BY subject LIMIT ?2"
        };
        let mut sources = tx
            .prepare_cached(sql)?
            .query_map(params![cursor, limit + 1], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        complete = sources.len() <= limit;
        sources.truncate(limit);
        for (subject, run, generation, status) in &sources {
            tx.execute(UPSERT_SOURCE, params![subject, run, generation, status])?;
        }
        processed = sources.len();
        tx.execute("UPDATE local_progress_capture SET complete=?1,cursor=COALESCE(?2,cursor) WHERE singleton=1",params![complete,sources.last().map(|s|&s.0)])?;
    }
    // A separate durable cursor captures zero-step runs. Both kinds share one work budget.
    let remaining = limit - processed;
    if !runs_complete && remaining > 0 {
        let sql = if run_cursor.is_some() {
            "SELECT id FROM mission_runs WHERE id>?1 ORDER BY id LIMIT ?2"
        } else {
            "SELECT id FROM mission_runs WHERE ?1 IS NULL ORDER BY id LIMIT ?2"
        };
        let mut runs = tx
            .prepare_cached(sql)?
            .query_map(params![run_cursor, remaining + 1], |row| {
                row.get::<_, String>(0)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        runs_complete = runs.len() <= remaining;
        runs.truncate(remaining);
        for run in &runs {
            tx.execute(
                "INSERT INTO local_progress_pending VALUES(?1) ON CONFLICT DO NOTHING",
                [run],
            )?;
        }
        processed += runs.len();
        tx.execute("UPDATE local_progress_run_capture SET complete=?1,cursor=COALESCE(?2,cursor) WHERE singleton=1",params![runs_complete,runs.last()])?;
    }
    Ok((processed, complete && runs_complete))
}

fn captured(connection: &Connection) -> Result<bool> {
    Ok(connection.query_row("SELECT s.complete AND r.complete FROM local_progress_capture s,local_progress_run_capture r WHERE s.singleton=1 AND r.singleton=1",[],|row|row.get(0))?)
}
fn require_fenced(connection: &Connection) -> Result<()> {
    let stored_ready: bool =
        connection.query_row("SELECT ready FROM ivm_views WHERE name=?1", [VIEW], |row| {
            row.get(0)
        })?;
    anyhow::ensure!(
        !stored_ready,
        "progress backfill requires a fenced view, not a pending source cut"
    );
    Ok(())
}

pub(crate) fn clean(connection: &Connection) -> Result<bool> {
    Ok(captured(connection)?
        && !connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM local_progress_pending)",
            [],
            |row| row.get::<_, bool>(0),
        )?)
}
fn ready(
    connection: &Connection,
    views: &Views,
    require_clean: bool,
) -> Result<smallclaims::ivm::SourceCut> {
    let cut = source_cut(connection)?.context("progress source unavailable")?;
    anyhow::ensure!(
        matches!(
            views.readiness(connection, VIEW, cut.epoch)?,
            Readiness::Ready(_)
        ),
        "progress counts unavailable"
    );
    anyhow::ensure!(
        captured(connection)?,
        "progress inventory capture incomplete"
    );
    anyhow::ensure!(
        !require_clean || clean(connection)?,
        "progress source changes pending"
    );
    Ok(cut)
}
/// Writer-only affected-run output maintenance. The shared owner supplies a completely
/// certified source cut; frontier equality alone cannot establish all projection hooks.
pub(crate) fn flush(tx: &Transaction<'_>, views: &Views, at: u128, limit: usize) -> Result<usize> {
    anyhow::ensure!((1..=1024).contains(&limit), "progress flush page bound");
    let mut cut = ready(tx, views, false)?;
    let runs = tx
        .prepare_cached("SELECT run_id FROM local_progress_pending ORDER BY run_id LIMIT ?1")?
        .query_map([limit], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for run in &runs {
        let key = format!("mission-run/{run}");
        cut.local_generation = cut
            .local_generation
            .checked_add(1)
            .context("progress generation overflow")?;
        let changes = views.local_change(
            tx,
            &LocalChange {
                kind: SOURCE.into(),
                old_keys: BTreeSet::from([key.clone()]),
                new_keys: BTreeSet::from([key]),
                evaluation_time_unix_ms: at,
            },
            cut,
        )?;
        anyhow::ensure!(changes.deferred.is_empty(), "progress maintenance deferred");
        tx.execute("DELETE FROM local_progress_pending WHERE run_id=?1", [run])?;
    }
    Ok(runs.len())
}
/// Explicit backfill output work while primitive readiness remains fenced. No events or
/// availability are published; initial snapshot invalidation belongs to the source owner.
pub(crate) fn backfill_page(tx: &Transaction<'_>, views: &Views, limit: usize) -> Result<usize> {
    anyhow::ensure!((1..=1024).contains(&limit), "progress backfill page bound");
    let cut = source_cut(tx)?.context("progress source unavailable")?;
    // Validate that this registry knows the name; pending admission is not a backfill fence.
    views.readiness(tx, VIEW, cut.epoch)?;
    require_fenced(tx)?;
    let runs = tx
        .prepare_cached("SELECT run_id FROM local_progress_pending ORDER BY run_id LIMIT ?1")?
        .query_map([limit], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for run in &runs {
        maintain(tx, &format!("mission-run/{run}"))?;
        tx.execute("DELETE FROM local_progress_pending WHERE run_id=?1", [run])?;
    }
    Ok(runs.len())
}
pub(crate) fn rows(
    connection: &Connection,
    views: &Views,
    runs: &[String],
) -> Result<BTreeMap<String, Progress>> {
    anyhow::ensure!(runs.len() <= 501, "progress selected batch bound");
    ready(connection, views, true)?;
    let ids = runs
        .iter()
        .map(|run| run.trim_start_matches("mission-run/"))
        .collect::<Vec<_>>();
    let rows=connection.prepare_cached("SELECT run_id,generation_id,total,done FROM local_progress_rows WHERE run_id IN (SELECT value FROM json_each(?1))")?.query_map([serde_json::to_string(&ids)?],|row|Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,u64>(2)?,row.get::<_,u64>(3)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows
        .into_iter()
        .map(|(id, generation, total, done)| {
            (
                format!("mission-run/{id}"),
                Progress {
                    generation: format!("run-generation/{generation}"),
                    total,
                    done,
                },
            )
        })
        .collect())
}
