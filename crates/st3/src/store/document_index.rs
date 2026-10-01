//! Canonical binding keys materialized at admission, so latest/history reads use one index.
use super::*;

pub(super) fn initialize(connection: &Connection) -> Result<()> {
    let transaction = connection.unchecked_transaction()?;
    let has_key: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('documents') WHERE name='binding_key')",
        [],
        |row| row.get(0),
    )?;
    if !has_key {
        transaction.execute_batch(
            "ALTER TABLE documents ADD COLUMN binding_key BLOB NOT NULL DEFAULT x'';",
        )?;
    }
    transaction.execute_batch(
        "CREATE INDEX IF NOT EXISTS document_canonical_latest ON documents(name,binding_key DESC);",
    )?;
    let done: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM meta WHERE key='canonical_document_binding_keys')",
        [],
        |row| row.get(0),
    )?;
    if !done {
        // Choose the earliest binding for each name/hash, independent of historical arrival.
        let mut bindings = BTreeMap::<(String, String), (Vec<u8>, String)>::new();
        let mut statement = transaction.prepare(
            "SELECT id,body FROM claims WHERE kind='doc.bound' AND NOT EXISTS(
                SELECT 1 FROM replica_records WHERE claim_id=claims.id AND state='repaired')",
        )?;
        for row in statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })? {
            let (id, body) = row?;
            let body: Value = serde_json::from_str(&body)?;
            let (Some(name), Some(hash)) = (
                body.get("name").and_then(Value::as_str),
                body.get("hash").and_then(Value::as_str),
            ) else {
                continue;
            };
            let key = canonical::sortable_key(&canonical::claim_key(&transaction, &id)?);
            let value = bindings
                .entry((name.into(), hash.into()))
                .or_insert_with(|| (key.clone(), id.clone()));
            if key < value.0 {
                *value = (key, id);
            }
        }
        drop(statement);
        let mut update = transaction.prepare(
            "UPDATE documents SET binding_claim_id=?3,binding_key=?4 WHERE name=?1 AND hash=?2",
        )?;
        for ((name, hash), (key, id)) in bindings {
            update.execute(params![name, hash, id, key])?;
        }
        drop(update);
        transaction.execute(
            "INSERT INTO meta(key,value) VALUES('canonical_document_binding_keys','1')",
            [],
        )?;
    }
    transaction.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn encoded_keys_preserve_the_complete_canonical_order(
            left in (any::<u128>(),any::<String>(),any::<u64>(),any::<String>(),any::<u64>(),any::<String>()),
            right in (any::<u128>(),any::<String>(),any::<u64>(),any::<String>(),any::<u64>(),any::<String>()),
        ) {
            // Also tie each earlier component in turn, to exercise every string terminator
            // and numeric boundary rather than usually deciding on acceptance time alone.
            let mut tied = right;
            for component in 0..6 {
                prop_assert_eq!(left.cmp(&tied),canonical::sortable_key(&left).cmp(&canonical::sortable_key(&tied)));
                match component { 0=>tied.0=left.0,1=>tied.1=left.1.clone(),2=>tied.2=left.2,3=>tied.3=left.3.clone(),4=>tied.4=left.4,_=>tied.5=left.5.clone() }
            }
        }
    }

    #[test]
    fn fleet_sized_document_reads_use_indexed_canonical_bindings_within_budget() {
        let store = Store::open_memory("alder").unwrap();
        let mut first = None;
        {
            let mut connection = store.connection.lock().unwrap();
            let transaction = connection.transaction().unwrap();
            let noise = append_claim_tx(
                &transaction,
                "alder",
                "daemon/alder",
                "daemon.diagnostic",
                None,
                &json!({"severity":"warning","code":"fixture","reason":"generated history"}),
                &[],
                None,
            )
            .unwrap();
            // Generate unrelated history directly: the measured path is reading, not batching
            // writes. The claim source digests still cover all 250,000 generated rows.
            transaction.execute(
                "WITH RECURSIVE fixture(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM fixture WHERE n<250000)
                 INSERT INTO claims(id,batch_id,subject,kind,origin,body,predecessors,accepted_at_unix_ms)
                 SELECT 'fixture-'||n,?1,'daemon/alder','daemon.diagnostic','alder',?2,'[]',?3 FROM fixture",
                params![noise.batch_id,canonical_json_text(&noise.body).unwrap(),noise.accepted_at_unix_ms.to_string()],
            ).unwrap();
            for index in 0..10_000 {
                let name = format!("doc/generated/{:03}", index % 200);
                let bytes = format!("version {index}");
                let hash = hex::encode(Sha256::digest(bytes.as_bytes()));
                transaction
                    .execute(
                        "INSERT INTO blobs VALUES(?1,?2,?3)",
                        params![hash, bytes.as_bytes(), bytes.len()],
                    )
                    .unwrap();
                let claim = append_claim_tx(
                    &transaction,
                    "alder",
                    &name,
                    "doc.bound",
                    None,
                    &json!({"name":name,"hash":hash,"size":bytes.len()}),
                    &[],
                    None,
                )
                .unwrap();
                select_replicated_document(&transaction, &claim, claim.store_index).unwrap();
                if index == 0 {
                    first = Some((name, hash));
                }
            }
            transaction.commit().unwrap();
        }
        let started = std::time::Instant::now();
        let latest = store
            .list_documents_page(None, None, false, None, 500)
            .unwrap();
        assert_eq!(latest.len(), 200);
        assert!(latest.iter().all(|version| version.latest));
        for version in &latest {
            let number: usize = version.name.rsplit('/').next().unwrap().parse().unwrap();
            assert_eq!(
                store.get_blob(&version.hash).unwrap().unwrap(),
                format!("version {}", 9800 + number).as_bytes()
            );
            assert_eq!(
                store
                    .latest_document_hash(&version.name)
                    .unwrap()
                    .as_deref(),
                Some(version.hash.as_str())
            );
        }
        let history = store
            .list_documents_page(None, None, true, None, 1000)
            .unwrap();
        assert_eq!(history.len(), 1000);
        let cursor = history.last().unwrap();
        let next = store
            .list_documents_page(
                None,
                None,
                true,
                Some((&cursor.name, cursor.created_index)),
                1000,
            )
            .unwrap();
        assert_eq!(next.len(), 1000);
        assert!(
            !next
                .iter()
                .any(|version| version.binding_claim_id == cursor.binding_claim_id)
        );
        let (name, hash) = first.unwrap();
        assert!(
            !find_document(&store.readers.get(), &name, &hash)
                .unwrap()
                .unwrap()
                .latest
        );
        eprintln!(
            "fleet document reads: {:?} over 260,000 claims / 10,000 versions",
            started.elapsed()
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "document reads took {:?} over 260,000 claims",
            started.elapsed()
        );
        let connection = store.readers.get();
        let plans=connection.prepare("EXPLAIN QUERY PLAN SELECT hash FROM documents WHERE name=?1 ORDER BY binding_key DESC LIMIT 1").unwrap()
            .query_map([name],|row|row.get::<_,String>(3)).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap();
        assert!(
            plans
                .iter()
                .any(|plan| plan.contains("document_canonical_latest")),
            "{plans:?}"
        );
        assert!(
            !plans.iter().any(|plan| plan.contains("TEMP B-TREE")),
            "{plans:?}"
        );
    }

    #[test]
    fn schema_fourteen_document_bindings_gain_keys_once_on_upgrade() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("claims.sqlite3");
        let store = Store::open(&path, "alder").unwrap();
        let first = store
            .put_document("doc/example", b"first", &None, "first")
            .unwrap();
        let second = store
            .put_document(
                "doc/example",
                b"second",
                &Some(first.binding_claim_id),
                "second",
            )
            .unwrap();
        drop(store);
        let connection = Connection::open(&path).unwrap();
        configure_projection_writer(&connection).unwrap();
        connection
            .execute_batch(
                "DROP TRIGGER projection_digest_documents_insert;
            DROP TRIGGER projection_digest_documents_update;
            DROP TRIGGER projection_digest_documents_delete;
            DROP INDEX document_canonical_latest;
            ALTER TABLE documents DROP COLUMN binding_key;
            DELETE FROM meta WHERE key='canonical_document_binding_keys';
            PRAGMA user_version=14;",
            )
            .unwrap();
        drop(connection);
        for _ in 0..2 {
            let store = Store::open(&path, "alder").unwrap();
            let versions = store.list_documents(None, true, 10).unwrap();
            assert_eq!(versions.len(), 2);
            assert_eq!(versions[0].hash, second.hash);
            assert!(versions[0].latest);
            assert!(!versions[1].latest);
            let connection = store.readers.get();
            assert_eq!(
                projection_digest::tables(&connection).unwrap(),
                projection_digest::oracle(&connection).unwrap()
            );
        }
    }
}
