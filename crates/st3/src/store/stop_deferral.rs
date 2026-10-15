//! Retained stops remain runtime work even when their owner is terminal. Read only the named
//! seats' diagnostic/fault histories; unrelated harness diagnostics do not end a deferral.
//!
//! A trigger copies each family's claims into `stop_deferral_claims` as they are inserted, so
//! opening a store never scans claims to build an index. Claims older than the trigger
//! (at or below the recorded ceiling) are copied once after the API serves, by a paced scan on
//! a reader. Until that scan commits, reads also look those claims up through the subject/kind
//! index, so they return the same rows either way.
use super::*;

/// (family, claim kind, predicate on `body`, recovered failure reference)
const FAMILIES: [(&str, &str, &str, &str); 3] = [
    (
        "diagnostic",
        "harness.diagnostic",
        "json_extract(body,'$.fields.code') IN ('stop-deferred','stop-deferral-cleared')",
        "''",
    ),
    (
        "failure",
        "operational.failure",
        "json_extract(body,'$.fields.condition')='stop-deferred'",
        "''",
    ),
    (
        "recovered",
        "operational.recovered",
        "1",
        "COALESCE(json_extract(body,'$.fields.failure'),'')",
    ),
];

/// Store indexes per backfill page. A cold page reads only these kinds' rows; on a copy of a
/// 16 GB production store the whole scan read about 26k rows in under two seconds.
const BACKFILL_PAGE: u64 = 16_384;

fn family(name: &str) -> (&'static str, &'static str, &'static str, &'static str) {
    *FAMILIES
        .iter()
        .find(|family| family.0 == name)
        .expect("a known stop-deferral family")
}

pub(super) fn create_schema(connection: &Connection) -> Result<()> {
    let arms = |column: usize| {
        FAMILIES
            .iter()
            .map(|family| {
                let (name, kind, predicate, reference) = *family;
                let value = [name, reference][column];
                format!(
                    "WHEN NEW.kind='{kind}' AND {} THEN {}",
                    predicate.replace("body", "NEW.body"),
                    if column == 0 { format!("'{value}'") } else { value.replace("body", "NEW.body") }
                )
            })
            .collect::<Vec<_>>()
            .join(" ")
    };
    let kinds = FAMILIES
        .iter()
        .map(|family| format!("'{}'", family.1))
        .collect::<Vec<_>>()
        .join(",");
    // The trigger and the ceiling commit together: every claim above the ceiling was copied
    // by the trigger, and every claim at or below it is the backfill's.
    connection.execute_batch(&format!(
        /* sqlite-large-value: allow-for-now lv-h77ec392fd407 owner=st3::store::stop_deferral TODO=chunk-or-reference-variable-columns */ "SAVEPOINT stop_deferral_schema;
         DROP INDEX IF EXISTS claims_stop_deferral_diagnostic_index;
         DROP INDEX IF EXISTS claims_stop_deferral_failure_index;
         DROP INDEX IF EXISTS claims_stop_deferral_recovery_index;
         CREATE TABLE IF NOT EXISTS stop_deferral_claims (
             subject TEXT NOT NULL,
             family TEXT NOT NULL,
             reference TEXT NOT NULL,
             claim_id TEXT NOT NULL,
             PRIMARY KEY(subject,family,reference,claim_id)
         ) WITHOUT ROWID;
         CREATE TABLE IF NOT EXISTS local_stop_deferral_backfill (
             singleton INTEGER PRIMARY KEY CHECK(singleton=1),
             ceiling INTEGER NOT NULL,
             complete INTEGER NOT NULL
         );
         INSERT OR IGNORE INTO local_stop_deferral_backfill
             SELECT 1,COALESCE(MAX(store_index),0),MAX(store_index) IS NULL FROM claims;
         CREATE TRIGGER IF NOT EXISTS stop_deferral_claims_copy AFTER INSERT ON claims
         WHEN NEW.kind IN ({kinds})
         BEGIN
             INSERT OR IGNORE INTO stop_deferral_claims(subject,family,reference,claim_id)
             SELECT NEW.subject,family,reference,NEW.id FROM (SELECT
                 CASE {} END AS family, CASE {} END AS reference)
             WHERE family IS NOT NULL;
         END;
         RELEASE stop_deferral_schema;",
        arms(0),
        arms(1),
    ))?;
    Ok(())
}

/// Claim ids of one family's claims for `subject`. The table is keyed by claim id because store
/// indexes are local and a checkpoint proof renumbers them. Claims older than the trigger come
/// from the subject/kind index until the backfill commits; after that the second arm is an
/// empty range.
fn family_sql(name: &str, subject: &str, reference: Option<&str>) -> String {
    let (name, kind, predicate, reference_sql) = family(name);
    let (copied, scanned) = match reference {
        Some(reference) => (
            format!(" AND reference={reference}"),
            format!(" AND {}={reference}", reference_sql.replace("body", "history.body")),
        ),
        None => (" AND reference=''".into(), String::new()),
    };
    format!(
        "SELECT claim_id FROM stop_deferral_claims
           WHERE subject={subject} AND family='{name}'{copied}
         UNION
         SELECT history.id FROM claims history INDEXED BY claims_subject_kind_index
           WHERE history.subject={subject} AND history.kind='{kind}'
             AND history.store_index<=(SELECT CASE WHEN complete THEN 0 ELSE ceiling END
               FROM local_stop_deferral_backfill)
             AND {}{scanned}",
        predicate.replace("body", "history.body"),
    )
}

impl Store {
    pub(crate) fn latest_stop_deferral(&self, subject: &str) -> Result<Option<ClaimRecord>> {
        smallclaims::touched::note_read(|| subject.to_owned());
        let connection = self.readers.get();
        connection.prepare_cached(/* sqlite-large-value: allow-for-now lv-hef8c4faf1b12 owner=st3::store::stop_deferral TODO=trace-statement-and-value-representation */ &canonical_sql(&format!(
            "SELECT {CLAIM_COLUMNS} FROM claims JOIN batches ON batches.id=claims.batch_id
             WHERE claims.id IN ({})
             ORDER BY CANONICAL_DESC(claims) LIMIT 1",
            family_sql("diagnostic", "?1", None),
        )))?.query_row(/* sqlite-large-value: allow-for-now lv-h450cd44a4030 owner=st3::store::stop_deferral TODO=trace-statement-and-value-representation */ [subject], claim_from_row).optional().map_err(Into::into)
    }

    pub(crate) fn pending_stop_deferral_failure(
        &self,
        diagnostic: &ClaimRecord,
    ) -> Result<Option<ClaimRecord>> {
        smallclaims::touched::note_read(|| diagnostic.subject.clone());
        let connection = self.readers.get();
        connection
            .prepare_cached(/* sqlite-large-value: allow-for-now lv-h2c64e927baa7 owner=st3::store::stop_deferral TODO=trace-statement-and-value-representation */ &canonical_sql(&format!(
                "SELECT {CLAIM_COLUMNS} FROM claims JOIN batches ON batches.id=claims.batch_id
             WHERE claims.id IN ({})
               AND json_extract(claims.body,'$.fields.episode')=?2
               AND NOT EXISTS ({})
             ORDER BY CANONICAL_DESC(claims) LIMIT 1",
                family_sql("failure", "?1", None),
                family_sql("recovered", "?1", Some("claims.id")),
            )))?
            .query_row(
                /* sqlite-large-value: allow-for-now lv-h49a00d947ee9 owner=st3::store::stop_deferral TODO=trace-statement-and-value-representation */ params![
                    diagnostic.subject,
                    format!("stop-deferred:{}", diagnostic.id)
                ],
                claim_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    /// Current aged deferral faults for this page of seats at the caller's history cut. A
    /// stopped runtime, replaced declaration, cleared diagnostic or recovered fault hides the
    /// episode immediately, even before the next reconciler pass.
    pub(crate) fn stop_deferral_faults_for(
        &self,
        subjects: &[String],
        at_index: u64,
    ) -> Result<BTreeMap<String, String>> {
        let connection = self.readers.get();
        let mut statement = connection.prepare_cached(/* sqlite-large-value: allow-for-now lv-h92fb007cecbd owner=st3::store::stop_deferral TODO=trace-statement-and-value-representation */ &canonical_sql(&format!(
            "WITH seats AS MATERIALIZED (
               SELECT desired.subject,desired.claim_id,
                 (SELECT claims.id FROM claims JOIN batches ON batches.id=claims.batch_id
                   WHERE claims.id IN ({}) AND +claims.store_index<=?2
                   ORDER BY CANONICAL_DESC(claims) LIMIT 1) AS diagnostic,
                 (SELECT claims.id FROM claims JOIN batches ON batches.id=claims.batch_id
                   WHERE claims.id IN ({}) AND +claims.store_index<=?2
                   ORDER BY CANONICAL_DESC(claims) LIMIT 1) AS failure,
                 (SELECT claims.body FROM claims INDEXED BY claims_subject_kind_accepted_index
                   JOIN batches ON batches.id=claims.batch_id
                   WHERE claims.subject=desired.subject AND claims.kind='runtime.observed'
                     AND +claims.store_index<=?2
                     AND NOT EXISTS (SELECT 1 FROM replica_records repaired
                       WHERE repaired.claim_id=claims.id AND repaired.state='repaired')
                   ORDER BY CANONICAL_DESC(claims) LIMIT 1) AS runtime
               FROM desired JOIN mission_runs owner ON owner.id=substr(desired.owner_run,13)
               WHERE desired.subject IN (SELECT value FROM json_each(?1))
                 AND substr(desired.owner_run,1,12)='mission-run/'
                 AND (owner.phase='terminal' OR owner.status IN ('completed','failed','cancelled'))
             )
             SELECT seats.subject,json_extract(fault.body,'$.fields.reason')
             FROM seats JOIN claims diagnostic ON diagnostic.id=seats.diagnostic
               JOIN claims fault ON fault.id=seats.failure
             WHERE json_extract(diagnostic.body,'$.fields.code')='stop-deferred'
               AND json_extract(fault.body,'$.fields.episode')='stop-deferred:' || diagnostic.id
               AND json_extract(fault.body,'$.fields.source_revision')=seats.claim_id
               AND json_extract(diagnostic.body,'$.evidence[0]')=seats.claim_id
               AND json_extract(seats.runtime,'$.fields.status')='running'
               AND json_extract(diagnostic.body,'$.fields.incarnation_id') IS json_extract(seats.runtime,'$.fields.incarnation_id')
               AND json_extract(fault.body,'$.fields.incarnation') IS json_extract(seats.runtime,'$.fields.incarnation_id')
               AND NOT EXISTS (SELECT 1 FROM claims recovered
                 WHERE recovered.id IN ({}) AND recovered.store_index<=?2)
             ORDER BY seats.subject",
            family_sql("diagnostic", "desired.subject", None),
            family_sql("failure", "desired.subject", None),
            family_sql("recovered", "seats.subject", Some("fault.id")),
        )))?;
        let mut faults = BTreeMap::new();
        for chunk in subjects.chunks(256) {
            let rows = statement
                .query_map(params![serde_json::to_string(chunk)?, at_index], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?;
            faults.extend(rows.collect::<rusqlite::Result<Vec<_>>>()?);
        }
        Ok(faults)
    }

    /// Copy the families' claims that predate the trigger, then mark the copy complete. Pages
    /// read through the kind index on a reader and `pause` runs between them; the only writer
    /// work is one transaction at the end. A restart before it commits scans again from the start.
    pub fn backfill_stop_deferral_history(&self, mut pause: impl FnMut()) -> Result<usize> {
        let (ceiling, complete): (u64, bool) = self.readers.get().query_row(
            "SELECT ceiling,complete FROM local_stop_deferral_backfill WHERE singleton=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if complete {
            return Ok(0);
        }
        let mut rows = Vec::new();
        let mut cursor = 0;
        while cursor < ceiling {
            let high = cursor.saturating_add(BACKFILL_PAGE).min(ceiling);
            {
                let connection = self.readers.get();
                for (name, kind, predicate, reference) in FAMILIES {
                    let mut page = connection.prepare_cached(/* sqlite-large-value: allow-for-now lv-hb11f41bbb0f2 owner=st3::store::stop_deferral TODO=trace-statement-and-value-representation */ &format!(
                        "SELECT subject,{reference},id FROM claims INDEXED BY claims_kind_index
                         WHERE kind='{kind}' AND store_index>?1 AND store_index<=?2 AND {predicate}"
                    ))?;
                    let found = page.query_map(params![cursor, high], |row| {
                        Ok((row.get::<_, String>(0)?, name, row.get::<_, String>(1)?, row.get::<_, String>(2)?))
                    })?;
                    rows.extend(found.collect::<rusqlite::Result<Vec<_>>>()?);
                }
            }
            cursor = high;
            if cursor < ceiling {
                pause();
            }
        }
        let mut connection = self.connection.write_background();
        let transaction = connection.transaction()?;
        {
            let mut insert = transaction.prepare_cached(
                /* sqlite-large-value: allow-for-now lv-he9ba5d624510 owner=st3::store::stop_deferral TODO=bound-or-reference-values-and-reclamation */ "INSERT OR IGNORE INTO stop_deferral_claims(subject,family,reference,claim_id)
                 VALUES (?1,?2,?3,?4)",
            )?;
            for (subject, name, reference, id) in &rows {
                insert.execute(/* sqlite-large-value: allow-for-now lv-h2d5c8c274c36 owner=st3::store::stop_deferral TODO=trace-statement-and-value-representation */ params![subject, name, reference, id])?;
            }
        }
        transaction.execute(
            /* sqlite-large-value: allow-for-now lv-h99ccb32baa5b owner=st3::store::stop_deferral TODO=bound-or-reference-values-and-reclamation */ "UPDATE local_stop_deferral_backfill SET complete=1 WHERE singleton=1",
            [],
        )?;
        transaction.commit()?;
        Ok(rows.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reopen_builds_no_claims_index_and_reads_history_before_and_after_the_copy() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("graph.db");
        let store = Store::open(&path, "node").unwrap();
        // Accepted times follow insertion order, so the newest claim is canonically latest.
        let accepted = std::cell::Cell::new(1_000_000_000_000_u64);
        let insert = |connection: &Connection, index: Option<u64>, id: &str, subject: &str, kind: &str, body: Value| {
            accepted.set(accepted.get() + 1);
            connection.execute(
                "INSERT INTO claims(store_index,id,batch_id,subject,kind,origin,body,predecessors,accepted_at_unix_ms)
                 VALUES (?1,?2,'batch/fixture',?3,?4,'fixture',?5,'[]',?6)",
                params![index, id, subject, kind, body.to_string(), accepted.get().to_string()],
            ).unwrap();
        };
        let (held, released) = ("agent/orchid/held", "agent/orchid/released");
        {
            let connection = store.connection.write();
            connection.execute("INSERT INTO batches(id,origin,replica_sequence,hash,accepted_at_unix_ms) VALUES ('batch/fixture','fixture',1,'fixture','1000000000000')", []).unwrap();
            // History spans several backfill pages, so the copy pauses between them.
            insert(&connection, Some(1), "unrelated", held, "harness.diagnostic", json!({"fields":{"code":"fixture-unrelated"}}));
            insert(&connection, Some(20_000), "held-diagnostic", held, "harness.diagnostic", json!({"fields":{"code":"stop-deferred"}}));
            insert(&connection, Some(40_000), "held-failure", held, "operational.failure", json!({"fields":{"condition":"stop-deferred","episode":"stop-deferred:held-diagnostic"}}));
            insert(&connection, Some(60_000), "released-diagnostic", released, "harness.diagnostic", json!({"fields":{"code":"stop-deferred"}}));
            insert(&connection, None, "released-failure", released, "operational.failure", json!({"fields":{"condition":"stop-deferred","episode":"stop-deferred:released-diagnostic"}}));
            insert(&connection, None, "released-recovery", released, "operational.recovered", json!({"fields":{"failure":"released-failure"}}));
            // The store as an older binary left it, including #2552's index.
            connection.execute_batch(
                "DROP TRIGGER stop_deferral_claims_copy;
                 DROP TABLE stop_deferral_claims;
                 DROP TABLE local_stop_deferral_backfill;
                 CREATE INDEX claims_stop_deferral_recovery_index
                   ON claims(subject,json_extract(body,'$.fields.failure')) WHERE kind='operational.recovered';",
            ).unwrap();
        }
        drop(store);

        let store = Store::open(&path, "node").unwrap();
        let state = |store: &Store| -> (u64, bool, u64) {
            store.readers.get().query_row(
                "SELECT ceiling,complete,(SELECT count(*) FROM stop_deferral_claims) FROM local_stop_deferral_backfill",
                [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            ).unwrap()
        };
        let ceiling = store.index().unwrap();
        assert_eq!(state(&store), (ceiling, false, 0), "open copies nothing");
        let indexes: Vec<String> = store.readers.get()
            .prepare("SELECT name FROM sqlite_schema WHERE type='index' AND name LIKE 'claims_stop_deferral%'").unwrap()
            .query_map([], |row| row.get(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap();
        assert!(indexes.is_empty(), "{indexes:?}");
        // A claim after the reopen is copied by the trigger.
        insert(&store.connection.write(), None, "released-cleared", released, "harness.diagnostic", json!({"fields":{"code":"stop-deferral-cleared"}}));
        assert_eq!(state(&store).2, 1);

        let reads = |store: &Store| {
            let held_diagnostic = store.latest_stop_deferral(held).unwrap().unwrap();
            let released_diagnostic = store.latest_stop_deferral(released).unwrap().unwrap();
            let released_failure = store.pending_stop_deferral_failure(
                &store.claim_by_id("released-diagnostic").unwrap().unwrap(),
            ).unwrap();
            (
                held_diagnostic.id.clone(),
                store.pending_stop_deferral_failure(&held_diagnostic).unwrap().map(|claim| claim.id),
                released_diagnostic.id,
                released_failure.map(|claim| claim.id),
            )
        };
        let expected = (
            "held-diagnostic".to_owned(),
            Some("held-failure".to_owned()),
            "released-cleared".to_owned(),
            None,
        );
        assert_eq!(reads(&store), expected, "before the copy");

        let mut pauses = 0;
        assert_eq!(store.backfill_stop_deferral_history(|| pauses += 1).unwrap(), 5);
        assert!(pauses >= 3, "{pauses}");
        assert_eq!(state(&store), (ceiling, true, 6));
        assert_eq!(reads(&store), expected, "after the copy");
        assert_eq!(store.backfill_stop_deferral_history(|| unreachable!()).unwrap(), 0);
        drop(store);
        let store = Store::open(&path, "node").unwrap();
        assert_eq!(state(&store), (ceiling, true, 6), "a later open keeps the copy");
        assert_eq!(reads(&store), expected);
    }

    #[test]
    fn stop_deferral_reads_do_not_scan_unrelated_diagnostic_history() {
        let store = Store::open_memory("node").unwrap();
        let sqlite_version: String = store
            .connection
            .write()
            .query_row("SELECT sqlite_version()", [], |row| row.get(0))
            .unwrap();
        eprintln!("SQLite {sqlite_version}");
        let subjects = (0..256)
            .map(|i| format!("agent/orchid/worker-{i:03}"))
            .collect::<Vec<_>>();
        {
            let connection = store.connection.write();
            connection.execute("INSERT INTO batches(id,origin,replica_sequence,hash,accepted_at_unix_ms) VALUES ('batch/fixture','fixture',1,'fixture','1000000000000')", []).unwrap();
            connection.execute("INSERT INTO mission_runs VALUES ('orchid','orchid','revision','generation','revision','orchid',NULL,'/tmp','person/operator','{}','run','completed','terminal','1000000000000','1000000000000')", []).unwrap();
            for (i, subject) in subjects.iter().enumerate() {
                let id = format!("{i:064x}");
                connection.execute("INSERT INTO claims(id,batch_id,subject,kind,origin,body,predecessors,accepted_at_unix_ms) VALUES (?1,'batch/fixture',?2,'intent.desired','fixture','{}','[]','1000000000000')", params![id,subject]).unwrap();
                connection.execute("INSERT INTO desired(subject,kind,revision,claim_id,body,owner_run) VALUES (?1,'stop','revision',?2,'{}','mission-run/orchid')", params![subject,id]).unwrap();
            }
        }
        let mut costs = Vec::new();
        let mut previous = 0;
        for count in [1_000, 10_000] {
            {
                let connection = store.connection.write();
                for i in previous..count {
                    connection.execute("INSERT INTO claims(id,batch_id,subject,kind,origin,body,predecessors,accepted_at_unix_ms) VALUES (?1,'batch/fixture',?2,'harness.diagnostic','fixture',?3,'[]',?4)", params![
                        format!("diagnostic-{i}"), subjects[0],
                        r#"{"fields":{"code":"fixture-unrelated","reason":"fixture"},"evidence":[]}"#,
                        (1_000_000_001_000_u64+i).to_string(),
                    ]).unwrap();
                }
            }
            previous = count;
            // Warm statement preparation before counting exactly the reads under review.
            assert!(store.latest_stop_deferral(&subjects[0]).unwrap().is_none());
            assert!(
                store
                    .stop_deferral_faults_for(&subjects, i64::MAX as u64)
                    .unwrap()
                    .is_empty()
            );
            let before = smallclaims::sqlite::work::total();
            let started = std::time::Instant::now();
            assert!(store.latest_stop_deferral(&subjects[0]).unwrap().is_none());
            assert!(
                store
                    .stop_deferral_faults_for(&subjects, i64::MAX as u64)
                    .unwrap()
                    .is_empty()
            );
            let cost = smallclaims::sqlite::work::total() - before;
            eprintln!(
                "unrelated diagnostics={count}, terminal-owner page=256: {cost:?}, wall={:?}",
                started.elapsed()
            );
            assert!(cost.vm_steps <= 80_000, "bounded page read: {cost:?}");
            costs.push(cost);
        }
        assert!(
            costs[1].vm_steps <= costs[0].vm_steps + 256,
            "history must not increase SQL work: {costs:?}"
        );
    }
}
