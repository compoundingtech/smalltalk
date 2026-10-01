//! Canonical binding keys materialized at admission, so latest/history reads use one index.
use super::*;

pub fn initialize(connection: &Connection) -> Result<()> {
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
