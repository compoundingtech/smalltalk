//! A single retained record diagnostic, addressed by its primary key.

use super::*;

const POINT_SQL: &str = "SELECT record_ref, writer, sequence, envelope_hash, position, state, claim_id,
                               subject_hint, kind_hint, error_code, error_message, replacement_claim_id
                        FROM replica_records WHERE record_ref=?1";

// Keep the point response identical to the same row in the collection response.
pub(super) fn decode(row: &rusqlite::Row<'_>) -> rusqlite::Result<ReplicaRecordView> {
    Ok(ReplicaRecordView {
        record_ref: row.get(0)?,
        writer: row.get(1)?,
        sequence: row.get(2)?,
        envelope_hash: row.get(3)?,
        position: row.get(4)?,
        state: row.get(5)?,
        claim_id: row.get(6)?,
        subject: row.get(7)?,
        kind: row.get(8)?,
        error_code: row.get(9)?,
        error_message: row.get(10)?,
        replacement_claim_id: row.get(11)?,
    })
}

impl Store {
    pub fn replica_record(&self, record_ref: &str) -> Result<Option<ReplicaRecordView>> {
        let connection = self.readers.get();
        Ok(connection
            .prepare_cached(/* sqlite-large-value: allow-for-now lv-hc203f8f75c66 owner=smallclaims::store::replica_record TODO=trace-statement-and-value-representation */ POINT_SQL)?
            .query_row(/* sqlite-large-value: allow-for-now lv-h7f473423a540 owner=smallclaims::store::replica_record TODO=trace-statement-and-value-representation */ [record_ref], decode)
            .optional()?)
    }
}

#[cfg(test)]
mod tests {
    use super::super::runtime::Plain;
    use super::*;
    use rusqlite::StatementStatus;

    fn insert(connection: &Connection, key: &str, sequence: i64, state: &str, details: bool) {
        connection.execute(
            "INSERT INTO replica_records(record_ref,writer,sequence,envelope_hash,position,raw,state,
                 claim_id,subject_hint,kind_hint,error_code,error_message,replacement_claim_id,
                 updated_at_unix_ms)
             VALUES(?1,'alder',?2,'envelope',0,X'',?3,?4,?5,?6,?7,?8,?9,'0')",
            params![key, sequence, state,
                details.then_some("claim/alder"), details.then_some("note/alder"),
                details.then_some("example.note"), details.then_some("fixture-error"),
                details.then_some("retained diagnostic"), details.then_some("claim/replacement")],
        ).unwrap();
    }

    #[test]
    fn point_matches_collection_fields_and_missing_keys_without_decoding_other_rows() {
        let store = Store::open_memory("alder", Arc::new(Plain)).unwrap();
        {
            let connection = store.connection.write();
            for (index, state) in ["pending", "valid", "unknown", "invalid", "repaired"]
                .iter()
                .enumerate()
            {
                insert(
                    &connection,
                    &format!("record/{state}"),
                    index as i64,
                    state,
                    index % 2 == 0,
                );
            }
        }
        let oracle = store.replica_records(false).unwrap();
        assert_eq!(oracle.len(), 5);
        for expected in oracle {
            let actual = store.replica_record(&expected.record_ref).unwrap().unwrap();
            assert_eq!(
                serde_json::to_value(actual).unwrap(),
                serde_json::to_value(expected).unwrap()
            );
        }
        for absent in ["record/missing", "record/valid' OR 1=1 --", ""] {
            assert!(store.replica_record(absent).unwrap().is_none());
        }

        // The old list-then-find path fails while decoding this unrelated corrupt row.
        store
            .connection
            .write()
            .execute(
                "UPDATE replica_records SET sequence=X'00' WHERE record_ref='record/invalid'",
                [],
            )
            .unwrap();
        assert!(store.replica_records(false).is_err());
        assert_eq!(
            store.replica_record("record/valid").unwrap().unwrap().state,
            "valid"
        );
        assert!(store.replica_record("record/missing").unwrap().is_none());
        assert!(
            store.replica_record("record/invalid").is_err(),
            "selected corruption must still fail"
        );
    }

    #[test]
    fn point_observes_committed_updates_deletes_rollback_and_reopen() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("claims.sqlite");
        {
            let store = Store::open(&path, "alder", Arc::new(Plain)).unwrap();
            insert(&store.connection.write(), "record/one", 1, "unknown", false);
            assert_eq!(
                store.replica_record("record/one").unwrap().unwrap().state,
                "unknown"
            );
            {
                let mut connection = store.connection.write();
                let transaction = connection.transaction().unwrap();
                transaction
                    .execute(
                        "UPDATE replica_records SET state='repaired',
                    replacement_claim_id='claim/replacement' WHERE record_ref='record/one'",
                        [],
                    )
                    .unwrap();
                transaction.commit().unwrap();
            }
            let repaired = store.replica_record("record/one").unwrap().unwrap();
            assert_eq!(repaired.state, "repaired");
            assert_eq!(
                repaired.replacement_claim_id.as_deref(),
                Some("claim/replacement")
            );
            {
                let mut connection = store.connection.write();
                let transaction = connection.transaction().unwrap();
                transaction
                    .execute(
                        "DELETE FROM replica_records WHERE record_ref='record/one'",
                        [],
                    )
                    .unwrap();
                transaction.rollback().unwrap();
            }
            assert_eq!(
                store.replica_record("record/one").unwrap().unwrap().state,
                "repaired"
            );
        }
        let store = Store::open(&path, "alder", Arc::new(Plain)).unwrap();
        assert_eq!(
            store.replica_record("record/one").unwrap().unwrap().state,
            "repaired"
        );
        store
            .connection
            .write()
            .execute(
                "DELETE FROM replica_records WHERE record_ref='record/one'",
                [],
            )
            .unwrap();
        assert!(store.replica_record("record/one").unwrap().is_none());
    }

    #[test]
    fn point_sql_uses_primary_key_without_scan_or_sort_as_unrelated_records_grow() {
        let store = Store::open_memory("alder", Arc::new(Plain)).unwrap();
        insert(
            &store.connection.write(),
            "record/selected",
            0,
            "valid",
            true,
        );
        fn work(store: &Store, key: &str) -> i32 {
            let reader = store.readers.get();
            let mut statement = reader.prepare(POINT_SQL).unwrap();
            statement.query_row([key], decode).optional().unwrap();
            assert_eq!(statement.get_status(StatementStatus::FullscanStep), 0);
            assert_eq!(statement.get_status(StatementStatus::Sort), 0);
            statement.get_status(StatementStatus::VmStep)
        }
        let before = (
            work(&store, "record/selected"),
            work(&store, "record/missing"),
        );
        {
            let mut connection = store.connection.write();
            let transaction = connection.transaction().unwrap();
            for index in 1..=4096 {
                insert(
                    &transaction,
                    &format!("record/unrelated/{index:04}"),
                    index,
                    "valid",
                    false,
                );
            }
            transaction.commit().unwrap();
        }
        assert_eq!(
            before,
            (
                work(&store, "record/selected"),
                work(&store, "record/missing")
            )
        );
        let reader = store.readers.get();
        let plan: Vec<String> = reader
            .prepare(&format!("EXPLAIN QUERY PLAN {POINT_SQL}"))
            .unwrap()
            .query_map(["record/selected"], |row| row.get(3))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(
            plan.iter()
                .any(|step| step.contains("SEARCH replica_records")
                    && step.contains("(record_ref=?)")),
            "{plan:?}"
        );
        assert!(
            plan.iter()
                .all(|step| !step.contains("SCAN ") && !step.contains("TEMP B-TREE")),
            "{plan:?}"
        );
    }
}
