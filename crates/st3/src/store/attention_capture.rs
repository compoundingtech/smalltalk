//! Bounded immutable native images at the Installer's exact deferred revision.
//! Admission only appends references and images. No reducer or dependency fanout
//! runs in a native trigger. Images remain until the source owner acknowledges
//! that every consumer has advanced past them. This helper does not inventory existing
//! native rows, qualify a source, or enable a production reader.
//! Ordinary main triggers own unqualified trigger DML; standalone reads/writes
//! bind main. Broader Installer calls remain unsupported under metadata shadows.
#![allow(dead_code)]
use anyhow::{Result, ensure};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use smallclaims::ivm::install::{Installer, Mutation, SourcePosition, capture::Change};

pub(crate) mod journal;

const MAX_TABLES: usize = 32;
const MAX_COLUMNS: usize = 64;
const MAX_IMAGE: usize = 64 * 1024;
const MAX_IMAGES: usize = 8192;
const MAX_BYTES: usize = 16 * 1024 * 1024;
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS main.local_attention_native_capture(
 source TEXT PRIMARY KEY,epoch INTEGER NOT NULL,managed INTEGER NOT NULL CHECK(managed IN(0,1)),
 image_rows INTEGER NOT NULL CHECK(image_rows>=0),image_bytes INTEGER NOT NULL CHECK(image_bytes>=0),
 reclaiming INTEGER NOT NULL DEFAULT 0 CHECK(reclaiming IN(0,1)));
CREATE TABLE IF NOT EXISTS main.local_attention_native_images(
 source TEXT NOT NULL,epoch INTEGER NOT NULL,revision INTEGER NOT NULL,side INTEGER NOT NULL CHECK(side IN(0,1)),
 native_table TEXT NOT NULL,body TEXT NOT NULL,bytes INTEGER NOT NULL,
 PRIMARY KEY(source,epoch,revision,side));";

#[derive(Clone, Copy)]
pub(crate) struct Table {
    pub name: &'static str,
    pub columns: &'static [&'static str],
    pub key: &'static [&'static str],
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use smallclaims::ivm::install::prepared::CaptureLimits;

    const TABLES: &[Table] = &[Table {
        name: "native",
        columns: &["id", "body", "number"],
        key: &["id"],
    }];
    fn fixture(rows: u64) -> Result<(Connection, Installer, NativeCapture)> {
        let mut c = Connection::open_in_memory()?;
        c.execute_batch("PRAGMA recursive_triggers=ON; CREATE TABLE native(id TEXT PRIMARY KEY,body BLOB UNIQUE,number INTEGER);")?;
        let installer = Installer::new(vec![])?;
        installer.create_schema(&c)?;
        let tx = c.transaction()?;
        installer.register_source(&tx, "attention-native", "native-images-test.v1", 7)?;
        let position = installer.position(&tx, "attention-native")?;
        installer.enable_deferred(
            &tx,
            &position,
            CaptureLimits {
                rows,
                bytes: 1024 * 1024,
                reference_bytes: 4096,
            },
        )?;
        let capture = NativeCapture::install(&tx, &installer, &position, TABLES)?;
        tx.commit()?;
        Ok((c, installer, capture))
    }
    fn count(c: &Connection, table: &str) -> Result<usize> {
        Ok(c.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))?)
    }
    #[test]
    fn immutable_images_cover_rekey_unique_replacement_and_rollback() -> Result<()> {
        let (mut c, installer, capture) = fixture(64)?;
        let tx = c.transaction()?;
        capture.begin(&tx, &installer)?;
        tx.execute(
            "INSERT INTO native VALUES('a',X'00FF',9223372036854775807)",
            [],
        )?;
        tx.execute("UPDATE native SET number=-1 WHERE id='a'", [])?;
        tx.execute("UPDATE native SET id='b' WHERE id='a'", [])?;
        tx.execute("INSERT OR REPLACE INTO native VALUES('c',X'00FF',NULL)", [])?;
        capture.commit(&tx)?;
        tx.commit()?;
        assert_eq!(installer.position(&c, "attention-native")?.revision, 6);
        let first = capture.image(&c, 1, 1, "native")?;
        assert_eq!(first["number"], json!(["integer", i64::MAX]));
        assert_eq!(first["body"], json!(["blob", "00FF"]));
        assert_eq!(capture.image(&c, 2, 0, "native")?, first);
        assert_eq!(
            capture.image(&c, 3, 0, "native")?["id"],
            json!(["text", "a"])
        );
        assert_eq!(
            capture.image(&c, 4, 1, "native")?["id"],
            json!(["text", "b"])
        );
        assert_eq!(
            capture.image(&c, 5, 0, "native")?["id"],
            json!(["text", "b"])
        );
        assert_eq!(
            capture.image(&c, 6, 1, "native")?["number"],
            json!(["null", null])
        );
        assert_eq!(count(&c, "local_attention_native_images")?, 7);
        let tx = c.transaction()?;
        capture.begin(&tx, &installer)?;
        tx.execute("DELETE FROM native", [])?;
        // Rollback removes both references and immutable images, including scope.
        tx.rollback()?;
        assert_eq!(count(&c, "native")?, 1);
        assert_eq!(count(&c, "local_attention_native_images")?, 7);
        assert_eq!(installer.position(&c, "attention-native")?.revision, 6);
        assert!(capture.compatible(&c)?);
        Ok(())
    }
    #[test]
    fn raw_writes_and_oversized_images_preserve_admission_and_fence_source() -> Result<()> {
        let (c, installer, capture) = fixture(64)?;
        c.execute("INSERT INTO native VALUES('raw',X'01',0)", [])?;
        assert_eq!(count(&c, "native")?, 1);
        assert!(installer.position(&c, "attention-native").is_err());
        assert!(!capture.compatible(&c)?);
        assert_eq!(count(&c, "local_attention_native_images")?, 0);
        let (mut c, installer, capture) = fixture(64)?;
        let tx = c.transaction()?;
        capture.begin(&tx, &installer)?;
        tx.execute("INSERT INTO native VALUES('large',zeroblob(65536),0)", [])?;
        capture.commit(&tx)?;
        tx.commit()?;
        assert_eq!(count(&c, "native")?, 1);
        assert!(installer.position(&c, "attention-native").is_err());
        assert_eq!(count(&c, "local_attention_native_images")?, 0);
        Ok(())
    }
    #[test]
    fn journal_and_image_quotas_do_not_silently_drop_a_replacement_side() -> Result<()> {
        for image_quota in [false, true] {
            let (mut c, installer, capture) = fixture(if image_quota { 64 } else { 1 })?;
            let tx = c.transaction()?;
            capture.begin(&tx, &installer)?;
            tx.execute("INSERT INTO native VALUES('a',X'01',0)", [])?;
            if image_quota {
                tx.execute(
                    "UPDATE local_attention_native_capture SET image_rows=?1",
                    [MAX_IMAGES - 1],
                )?;
            }
            tx.execute("UPDATE native SET number=1", [])?;
            capture.commit(&tx)?;
            tx.commit()?;
            assert!(installer.position(&c, "attention-native").is_err());
            assert_eq!(count(&c, "native")?, 1);
            assert_eq!(count(&c, "local_attention_native_images")?, 1);
        }
        Ok(())
    }
    #[test]
    fn trigger_or_recursive_policy_changes_cannot_certify_coverage() -> Result<()> {
        let (mut c, installer, capture) = fixture(64)?;
        c.execute_batch("PRAGMA recursive_triggers=OFF;")?;
        assert!(!capture.compatible(&c)?);
        let tx = c.transaction()?;
        capture.begin(&tx, &installer)?;
        tx.commit()?;
        c.execute_batch("PRAGMA recursive_triggers=ON;")?;
        assert!(!capture.compatible(&c)?);
        let (mut c, installer, capture) = fixture(64)?;
        c.execute_batch(&format!("DROP TRIGGER {}", quoted(&capture.triggers[0].0)?))?;
        assert!(!capture.compatible(&c)?);
        let tx = c.transaction()?;
        capture.begin(&tx, &installer)?;
        tx.execute("INSERT INTO native VALUES('still-admitted',X'01',0)", [])?;
        tx.commit()?;
        assert!(installer.position(&c, "attention-native").is_err());
        assert_eq!(count(&c, "native")?, 1);
        Ok(())
    }

    #[test]
    fn capture_pages_charge_both_sides_and_keep_owned_typed_cells() -> Result<()> {
        let (mut c, installer, capture) = fixture(64)?;
        let tx = c.transaction()?;
        capture.begin(&tx, &installer)?;
        tx.execute(
            "INSERT INTO native VALUES('unicode',?1,-17)",
            ["\"\\\n東京"],
        )?;
        tx.execute("UPDATE native SET number=42", [])?;
        capture.commit(&tx)?;
        tx.commit()?;
        let position = installer.position(&c, "attention-native")?;
        let references = c
            .prepare("SELECT payload FROM main.ivm_install_journal ORDER BY revision")?
            .query_map([], |r| r.get::<_, String>(0))?
            .map(|row| Ok(serde_json::from_str::<Mutation>(&row?)?))
            .collect::<Result<Vec<_>>>()?;
        assert!(
            capture
                .capture_images(&c, &position, &references, 2, 256 * 1024)
                .is_err()
        );
        assert!(
            capture
                .capture_images(&c, &position, &references, 3, 1)
                .is_err()
        );
        let owned = capture.capture_images(&c, &position, &references, 3, 256 * 1024)?;
        assert_eq!(owned.len(), 3);
        drop(c);
        assert_eq!(
            owned[0].decode()?["number"],
            rusqlite::types::Value::Integer(-17)
        );
        assert_eq!(
            owned[2].decode()?["number"],
            rusqlite::types::Value::Integer(42)
        );
        assert_eq!(
            owned[0].decode()?["body"],
            rusqlite::types::Value::Text("\"\\\n東京".into())
        );
        Ok(())
    }

    #[test]
    fn raw_retained_image_changes_fence_only_the_owning_source() -> Result<()> {
        for action in [
            "UPDATE local_attention_native_images SET body='{}'",
            "DELETE FROM main.local_attention_native_images",
            "UPDATE local_attention_native_capture SET reclaiming=1; DELETE FROM main.local_attention_native_images",
            "INSERT INTO local_attention_native_images VALUES('attention-native',8,2,1,'native','{}',2)",
        ] {
            let (mut c, installer, capture) = fixture(64)?;
            let tx = c.transaction()?;
            installer.register_source(&tx, "other", "other.v1", 1)?;
            capture.begin(&tx, &installer)?;
            tx.execute("INSERT INTO native VALUES('a',X'01',0)", [])?;
            capture.commit(&tx)?;
            tx.commit()?;
            c.execute_batch(action)?;
            assert!(installer.position(&c, "attention-native").is_err());
            assert!(installer.position(&c, "other").is_ok());
            assert_eq!(count(&c, "native")?, 1);
        }
        Ok(())
    }
    #[test]
    fn rolled_back_schema_revision_cannot_mask_later_trigger_loss() -> Result<()> {
        let (mut c, installer, capture) = fixture(64)?;
        let tx = c.transaction()?;
        tx.execute_batch("CREATE TABLE unrelated(id TEXT PRIMARY KEY)")?;
        let checked: i64 = tx.query_row("PRAGMA schema_version", [], |r| r.get(0))?;
        assert!(capture.compatible(&tx)?);
        tx.rollback()?;
        let tx = c.transaction()?;
        tx.execute_batch(&format!("DROP TRIGGER {}", quoted(&capture.triggers[1].0)?))?;
        let reused: i64 = tx.query_row("PRAGMA schema_version", [], |r| r.get(0))?;
        assert_eq!(checked, reused);
        assert!(!capture.compatible(&tx)?);
        capture.begin(&tx, &installer)?;
        tx.commit()?;
        assert!(installer.position(&c, "attention-native").is_err());
        Ok(())
    }

    #[test]
    fn capture_uses_main_native_and_retained_images_despite_temp_shadows() -> Result<()> {
        let (mut c, installer, capture) = fixture(64)?;
        c.execute_batch(
            "CREATE TEMP TABLE native(other TEXT PRIMARY KEY);
             CREATE TEMP TABLE local_attention_native_capture AS
               SELECT * FROM main.local_attention_native_capture;
             CREATE TEMP TABLE local_attention_native_images AS
               SELECT * FROM main.local_attention_native_images;
             UPDATE temp.local_attention_native_capture SET managed=1,image_rows=8192;",
        )?;
        let tx = c.transaction()?;
        // Setup must validate main's columns and find the existing main triggers.
        let position = installer.position(&tx, "attention-native")?;
        let reopened = NativeCapture::install(&tx, &installer, &position, TABLES)?;
        reopened.begin(&tx, &installer)?;
        tx.execute("INSERT INTO main.native VALUES('main',X'00FF',17)", [])?;
        reopened.commit(&tx)?;
        tx.commit()?;
        assert!(capture.compatible(&c)?);
        assert_eq!(
            capture.image(&c, 1, 1, "native")?["number"],
            json!(["integer", 17])
        );
        assert_eq!(count(&c, "main.native")?, 1);
        assert_eq!(count(&c, "temp.native")?, 0);
        assert_eq!(count(&c, "main.local_attention_native_images")?, 1);
        assert_eq!(count(&c, "temp.local_attention_native_images")?, 0);
        let shadow_rows: usize = c.query_row(
            "SELECT image_rows FROM temp.local_attention_native_capture",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(shadow_rows, MAX_IMAGES);
        Ok(())
    }

    #[test]
    fn metadata_shadows_refuse_reclaim_and_main_fence_preserves_native_admission() -> Result<()> {
        let (mut c, installer, capture) = fixture(64)?;
        let reclaim = capture.capture_reclaim(&c, &installer, 64)?;
        let position = installer.position(&c, "attention-native")?;
        c.execute_batch(
            "CREATE TEMP TABLE ivm_install_sources AS SELECT * FROM main.ivm_install_sources;
             CREATE TEMP TABLE ivm_install_jobs AS SELECT * FROM main.ivm_install_jobs;
             CREATE TEMP TABLE ivm_install_roots AS SELECT * FROM main.ivm_install_roots;
             CREATE TEMP TABLE ivm_install_journal AS SELECT * FROM main.ivm_install_journal;
             CREATE TEMP TABLE ivm_install_deferred AS SELECT * FROM main.ivm_install_deferred;",
        )?;
        assert!(!capture.compatible(&c)?);
        assert!(
            capture
                .capture_images(&c, &position, &[], 64, 256 * 1024)
                .is_err()
        );
        assert!(capture.capture_reclaim(&c, &installer, 64).is_err());
        let tx = c.transaction()?;
        assert!(capture.publish_reclaim(&tx, &installer, &reclaim).is_err());
        capture.begin(&tx, &installer)?;
        tx.execute("INSERT INTO main.native VALUES('admitted',X'01',0)", [])?;
        capture.commit(&tx)?;
        tx.commit()?;
        let main_available: bool = c.query_row(
            "SELECT available FROM main.ivm_install_sources WHERE name='attention-native'",
            [],
            |r| r.get(0),
        )?;
        let shadow_available: bool = c.query_row(
            "SELECT available FROM temp.ivm_install_sources WHERE name='attention-native'",
            [],
            |r| r.get(0),
        )?;
        assert!(!main_available);
        assert!(shadow_available);
        assert_eq!(count(&c, "main.native")?, 1);
        assert_eq!(count(&c, "main.local_attention_native_images")?, 0);
        c.execute_batch(
            "DROP TABLE temp.ivm_install_sources;
            DROP TABLE temp.ivm_install_jobs; DROP TABLE temp.ivm_install_roots;
            DROP TABLE temp.ivm_install_journal; DROP TABLE temp.ivm_install_deferred;",
        )?;
        assert!(!capture.compatible(&c)?);
        Ok(())
    }
}

pub(crate) struct NativeCapture {
    source: String,
    epoch: u64,
    fingerprint: String,
    gap_main_sql: String,
    schema: Vec<(String, String)>,
    triggers: Vec<(String, String)>,
    tables: Vec<Table>,
}

/// Serialized typed cells are decoded only after the reader snapshot is released.
pub(crate) struct RetainedImage {
    pub revision: u64,
    pub side: usize,
    pub table: String,
    pub body: String,
}

impl RetainedImage {
    pub(crate) fn decode(
        &self,
    ) -> Result<std::collections::BTreeMap<String, rusqlite::types::Value>> {
        use rusqlite::types::Value as Cell;
        let value: serde_json::Value = serde_json::from_str(&self.body)?;
        let fields = value
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("attention image is not an object"))?;
        ensure!(fields.len() <= MAX_COLUMNS, "attention image column bound");
        fields
            .iter()
            .map(|(name, value)| {
                let pair = value
                    .as_array()
                    .filter(|pair| pair.len() == 2)
                    .ok_or_else(|| anyhow::anyhow!("attention image cell shape"))?;
                let cell = match pair[0].as_str() {
                    Some("null") if pair[1].is_null() => Cell::Null,
                    Some("integer") => Cell::Integer(
                        pair[1]
                            .as_i64()
                            .ok_or_else(|| anyhow::anyhow!("attention integer image"))?,
                    ),
                    Some("real") => Cell::Real(
                        pair[1]
                            .as_f64()
                            .filter(|n| n.is_finite())
                            .ok_or_else(|| anyhow::anyhow!("attention real image"))?,
                    ),
                    Some("text") => Cell::Text(
                        pair[1]
                            .as_str()
                            .ok_or_else(|| anyhow::anyhow!("attention text image"))?
                            .to_owned(),
                    ),
                    Some("blob") => Cell::Blob(hex::decode(
                        pair[1]
                            .as_str()
                            .ok_or_else(|| anyhow::anyhow!("attention blob image"))?,
                    )?),
                    _ => anyhow::bail!("attention image cell type"),
                };
                Ok((name.clone(), cell))
            })
            .collect()
    }
}

pub(crate) struct ReclaimPage {
    position: SourcePosition,
    image_rows: usize,
    image_bytes: usize,
    entries: Vec<(u64, usize, usize)>,
}

fn quoted(name: &str) -> Result<String> {
    ensure!(
        !name.is_empty()
            && name.len() <= 128
            && !name.as_bytes()[0].is_ascii_digit()
            && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'),
        "invalid attention capture identifier"
    );
    Ok(format!("\"{name}\""))
}
fn literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

// The broader Installer position/reclaim API is still unqualified. Refuse that
// connection shape before calling it; this does not validate physical schema or
// bound metadata VM work, and is not a source coverage certificate.
fn installer_metadata_unshadowed(c: &Connection) -> Result<bool> {
    Ok(!c.query_row(
        "SELECT EXISTS(SELECT 1 FROM temp.sqlite_schema WHERE type IN ('table','view')
         AND name IN ('ivm_install_sources','ivm_install_jobs','ivm_install_roots',
                      'ivm_install_journal','ivm_install_deferred'))",
        [],
        |r| r.get::<_, bool>(0),
    )?)
}

// Size before hex/JSON allocation; the final encoded size is checked as well.
fn image(table: &Table, prefix: &str) -> Result<(String, String)> {
    let mut fields = Vec::new();
    let mut sizes = Vec::new();
    let mut valid = Vec::new();
    for name in table.columns {
        let column = format!("{prefix}.{}", quoted(name)?);
        fields.push(literal(name));
        fields.push(format!("json_array(typeof({column}),CASE WHEN typeof({column})='blob' THEN hex({column}) ELSE {column} END)"));
        sizes.push(format!("{}+CASE typeof({column}) WHEN 'blob' THEN 2*length({column})+32 WHEN 'text' THEN 6*length(CAST({column} AS BLOB))+32 ELSE 64 END", name.len()*6+8));
        valid.push(format!(
            "(typeof({column})<>'real' OR abs({column})<=1.7976931348623157e308)"
        ));
    }
    let body = format!(
        "CASE WHEN ({}) AND ({})<={MAX_IMAGE} THEN json_object({}) ELSE NULL END",
        valid.join(" AND "),
        sizes.join("+"),
        fields.join(",")
    );
    let bounded = format!("({body}) IS NOT NULL AND length(CAST(({body}) AS BLOB))<={MAX_IMAGE}");
    Ok((body, bounded))
}

impl NativeCapture {
    /// Explicit setup against an already registered deferred source. Populated
    /// tables are allowed, but this does not seed them or attest a Ready cut.
    pub(crate) fn install(
        tx: &Transaction<'_>,
        installer: &Installer,
        position: &SourcePosition,
        tables: &[Table],
    ) -> Result<Self> {
        ensure!(
            installer_metadata_unshadowed(tx)?,
            "attention capture Installer metadata shadow"
        );
        ensure!(
            (1..=MAX_TABLES).contains(&tables.len()),
            "attention capture table bound"
        );
        ensure!(
            tx.query_row("PRAGMA recursive_triggers", [], |r| r.get::<_, bool>(0))?,
            "attention capture requires recursive replacement deletion"
        );
        tx.execute_batch(SCHEMA)?;
        tx.execute(
            "INSERT INTO main.local_attention_native_capture VALUES(?1,?2,0,0,0,0) ON CONFLICT DO NOTHING",
            params![position.source, position.epoch],
        )?;
        let mut capture = Self {
            source: position.source.clone(),
            epoch: position.epoch,
            fingerprint: position.fingerprint.clone(),
            gap_main_sql: String::new(),
            schema: vec![],
            triggers: vec![],
            tables: tables.to_vec(),
        };
        let source = literal(&position.source);
        let scope = format!(
            "EXISTS(SELECT 1 FROM main.local_attention_native_capture WHERE source={source} AND epoch={} AND managed=1)",
            position.epoch
        );
        let mut names = std::collections::BTreeSet::new();
        for table in tables {
            ensure!(
                names.insert(table.name),
                "duplicate attention capture table"
            );
            ensure!(
                (1..=MAX_COLUMNS).contains(&table.columns.len()),
                "attention capture column bound"
            );
            let table_name = format!("main.{}", quoted(table.name)?);
            let columns = tx
                .prepare("SELECT name FROM pragma_table_xinfo(?1,'main') ORDER BY cid LIMIT 65")?
                .query_map([table.name], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            ensure!(
                columns
                    .iter()
                    .map(String::as_str)
                    .eq(table.columns.iter().copied()),
                "attention capture requires all native columns in schema order"
            );
            let sql: String = tx.query_row(
                "SELECT sql FROM main.sqlite_schema WHERE type='table' AND name=?1",
                [table.name],
                |r| r.get(0),
            )?;
            capture.schema.push((table.name.into(), sql));
            let plan = installer.capture_plan(tx, position, table.name, table.key)?;
            // Execute the foundation's entire compiled fence body conditionally through
            // an empty view. Never duplicate or rewrite Installer SQL internals.
            if capture.triggers.is_empty() {
                capture.gap_main_sql = plan.gap_main_sql().to_owned();
                let view_sql = "CREATE VIEW local_attention_native_gap AS SELECT source,epoch FROM main.local_attention_native_capture WHERE 0";
                let previous: Option<String> = tx.query_row(
                    "SELECT sql FROM main.sqlite_schema WHERE type='view' AND name='local_attention_native_gap'",
                    [], |r| r.get(0),
                ).optional()?;
                if let Some(previous) = previous {
                    ensure!(previous == view_sql, "attention capture fence view changed");
                } else {
                    tx.execute_batch(view_sql)?;
                }
                capture
                    .schema
                    .push(("local_attention_native_gap".into(), view_sql.into()));
                let name = format!(
                    "attention_native_{}_gap",
                    &smallclaims::hash::canonical_hash(&serde_json::json!([
                        position.source,
                        position.epoch
                    ]))?[..16]
                );
                let sql = format!(
                    "CREATE TRIGGER {} INSTEAD OF INSERT ON main.local_attention_native_gap WHEN NEW.source={source} AND NEW.epoch={} BEGIN {} END",
                    quoted(&name)?,
                    position.epoch,
                    plan.gap_sql()
                );
                let previous: Option<String> = tx
                    .query_row(
                        "SELECT sql FROM main.sqlite_schema WHERE type='trigger' AND name=?1",
                        [&name],
                        |r| r.get(0),
                    )
                    .optional()?;
                if let Some(previous) = previous {
                    ensure!(previous == sql, "attention capture fence trigger changed");
                } else {
                    tx.execute_batch(&sql)?;
                }
                capture.triggers.push((name, sql));
                // Retained producer images cannot be changed or removed by a raw path.
                // Reclamation is the only managed removal path and remains transactional.
                for (suffix, action, condition) in [
                    (
                        "image_insert",
                        "INSERT",
                        format!(
                            "NEW.source={source} AND NOT (NEW.epoch={} AND {scope} AND NEW.revision={} AND {})",
                            position.epoch,
                            plan.revision_sql(),
                            plan.available_sql()
                        ),
                    ),
                    (
                        "image_update",
                        "UPDATE",
                        format!("OLD.source={source} OR NEW.source={source}"),
                    ),
                    (
                        "image_delete",
                        "DELETE",
                        format!(
                            "OLD.source={source} AND NOT (OLD.epoch={} AND EXISTS(SELECT 1 FROM main.local_attention_native_capture WHERE source={source} AND epoch={} AND reclaiming=1) AND OLD.revision<={} AND NOT EXISTS(SELECT 1 FROM main.ivm_install_journal WHERE source={source} AND revision<=OLD.revision))",
                            position.epoch,
                            position.epoch,
                            plan.revision_sql()
                        ),
                    ),
                ] {
                    let name = format!(
                        "attention_native_{}_{suffix}",
                        &smallclaims::hash::canonical_hash(&serde_json::json!([
                            position.source,
                            position.epoch
                        ]))?[..16]
                    );
                    let sql = format!(
                        "CREATE TRIGGER {} AFTER {action} ON main.local_attention_native_images WHEN {condition} BEGIN {} END",
                        quoted(&name)?,
                        plan.gap_sql()
                    );
                    let previous: Option<String> = tx
                        .query_row(
                            "SELECT sql FROM main.sqlite_schema WHERE type='trigger' AND name=?1",
                            [&name],
                            |r| r.get(0),
                        )
                        .optional()?;
                    if let Some(previous) = previous {
                        ensure!(previous == sql, "attention image guard changed");
                    } else {
                        tx.execute_batch(&sql)?;
                    }
                    capture.triggers.push((name, sql));
                }
                for name in [
                    "local_attention_native_capture",
                    "local_attention_native_images",
                ] {
                    let sql: String = tx.query_row(
                        "SELECT sql FROM main.sqlite_schema WHERE type='table' AND name=?1",
                        [name],
                        |r| r.get(0),
                    )?;
                    capture.schema.push((name.into(), sql));
                }
            }
            let same = table.key.iter().map(|k| {
                let k = quoted(k)?;
                Ok(format!("(OLD.{k} COLLATE BINARY IS NEW.{k} COLLATE BINARY AND typeof(OLD.{k})=typeof(NEW.{k}))"))
            }).collect::<Result<Vec<_>>>()?.join(" AND ");
            let retain = |prefix: &str, side: usize| -> Result<(String, String)> {
                let (body, bounded) = image(table, prefix)?;
                let check = format!(
                    "({bounded}) AND EXISTS(SELECT 1 FROM main.local_attention_native_capture WHERE source={source} AND epoch={} AND image_rows<{MAX_IMAGES} AND image_bytes<= {MAX_BYTES}-length(CAST(({body}) AS BLOB)))",
                    position.epoch
                );
                let append = format!(
                    "INSERT INTO local_attention_native_images SELECT {source},{},{},{side},{},({body}),length(CAST(({body}) AS BLOB)) WHERE {}; UPDATE local_attention_native_capture SET image_rows=image_rows+1,image_bytes=image_bytes+length(CAST(({body}) AS BLOB)) WHERE source={source} AND {};",
                    position.epoch,
                    plan.revision_sql(),
                    literal(table.name),
                    plan.available_sql(),
                    plan.available_sql()
                );
                Ok((check, append))
            };
            let (old_check, old) = retain("OLD", 0)?;
            let (new_check, new) = retain("NEW", 1)?;
            let guard = |check: &str| {
                let gap = format!("NOT ({scope} AND ({check}))");
                format!(
                    "INSERT INTO local_attention_native_gap SELECT {source},{} WHERE {gap};",
                    position.epoch
                )
            };
            // Replacement reserves both images before either is inserted.
            let pair_check = format!(
                "({old_check}) AND ({new_check}) AND EXISTS(SELECT 1 FROM main.local_attention_native_capture WHERE source={source} AND image_rows<={MAX_IMAGES}-2 AND image_bytes<={MAX_BYTES}-length(CAST(({}) AS BLOB))-length(CAST(({}) AS BLOB)))",
                image(table, "OLD")?.0,
                image(table, "NEW")?.0
            );
            for (suffix, action, condition, body) in [
                (
                    "insert",
                    "INSERT",
                    "1".into(),
                    format!(
                        "{}{}{}",
                        guard(&new_check),
                        plan.append_sql(Change::Insert),
                        new
                    ),
                ),
                (
                    "delete",
                    "DELETE",
                    "1".into(),
                    format!(
                        "{}{}{}",
                        guard(&old_check),
                        plan.append_sql(Change::Delete),
                        old
                    ),
                ),
                (
                    "replace",
                    "UPDATE",
                    same.clone(),
                    format!(
                        "{}{}{}{}",
                        guard(&pair_check),
                        plan.append_sql(Change::Replacement),
                        old,
                        new
                    ),
                ),
                (
                    "rekey",
                    "UPDATE",
                    format!("NOT ({same})"),
                    format!(
                        "{}{}{}{}{}",
                        guard(&pair_check),
                        plan.append_sql(Change::Delete),
                        old,
                        plan.append_sql(Change::Insert),
                        new
                    ),
                ),
            ] {
                let name = format!(
                    "attention_native_{}_{}_{}",
                    &smallclaims::hash::canonical_hash(&serde_json::json!([
                        position.source,
                        position.epoch
                    ]))?[..16],
                    table.name,
                    suffix
                );
                let sql = format!(
                    "CREATE TRIGGER {} AFTER {action} ON {table_name} WHEN {condition} BEGIN {body} END",
                    quoted(&name)?
                );
                ensure!(sql.len() <= 1024 * 1024, "attention capture SQL bound");
                let previous: Option<String> = tx
                    .query_row(
                        "SELECT sql FROM main.sqlite_schema WHERE type='trigger' AND name=?1",
                        [&name],
                        |r| r.get(0),
                    )
                    .optional()?;
                if let Some(previous) = previous {
                    ensure!(previous == sql, "attention capture trigger changed");
                } else {
                    tx.execute_batch(&sql)?;
                }
                capture.triggers.push((name, sql));
            }
        }
        ensure!(
            capture.compatible(tx)?,
            "attention capture source incompatible"
        );
        Ok(capture)
    }

    /// Paired writer hooks. A raw write outside this scope permanently fences
    /// the source in that write transaction, preserving native admission.
    pub(crate) fn begin(&self, tx: &Transaction<'_>, _installer: &Installer) -> Result<()> {
        let inactive = tx
            .query_row(
                "SELECT managed=0 FROM main.local_attention_native_capture WHERE source=?1 AND epoch=?2",
                params![self.source, self.epoch],
                |r| r.get::<_, bool>(0),
            )
            .optional()?
            .unwrap_or(false);
        if !inactive || !self.compatible(tx)? {
            // The library compiles this explicitly main-bound standalone fence.
            // Do not call unqualified Installer::source_gap under TEMP metadata.
            ensure!(
                !self.gap_main_sql.trim().is_empty(),
                "attention capture main fence missing"
            );
            tx.execute_batch(&self.gap_main_sql)?;
            return Ok(());
        }
        tx.execute(
            "UPDATE main.local_attention_native_capture SET managed=1 WHERE source=?1 AND epoch=?2",
            params![self.source, self.epoch],
        )?;
        Ok(())
    }
    pub(crate) fn commit(&self, tx: &Transaction<'_>) -> Result<()> {
        tx.execute(
            "UPDATE main.local_attention_native_capture SET managed=0 WHERE source=?1 AND epoch=?2",
            params![self.source, self.epoch],
        )?;
        Ok(())
    }
    pub(crate) fn compatible(&self, c: &Connection) -> Result<bool> {
        if !installer_metadata_unshadowed(c)? {
            return Ok(false);
        }
        if !c.query_row("PRAGMA recursive_triggers", [], |r| r.get::<_, bool>(0))? {
            return Ok(false);
        }
        let available: bool = c.query_row("SELECT EXISTS(SELECT 1 FROM main.ivm_install_sources s JOIN main.local_attention_native_capture c ON c.source=s.name AND c.epoch=s.epoch WHERE s.name=?1 AND s.epoch=?2 AND s.fingerprint=?3 AND s.available=1)",params![self.source,self.epoch,self.fingerprint],|r|r.get(0))?;
        if !available {
            return Ok(false);
        }
        for (name, expected) in self.schema.iter().chain(&self.triggers) {
            let actual: Option<String> = c
                .query_row(
                    "SELECT sql FROM main.sqlite_schema WHERE name=?1",
                    [name],
                    |r| r.get(0),
                )
                .optional()?;
            if actual.as_ref() != Some(expected) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    #[cfg(test)]
    pub(crate) fn image(
        &self,
        c: &Connection,
        revision: u64,
        side: usize,
        table: &str,
    ) -> Result<serde_json::Value> {
        ensure!(side <= 1, "attention image side");
        let body: String = c.query_row("SELECT body FROM main.local_attention_native_images WHERE source=?1 AND epoch=?2 AND revision=?3 AND side=?4 AND native_table=?5",params![self.source,self.epoch,revision,side,table],|r|r.get(0))?;
        Ok(serde_json::from_str(&body)?)
    }

    /// Capture a page of official Installer locators. Charge each replacement side and
    /// its serialized bytes before extracting any body; no read guard escapes this method.
    pub(crate) fn capture_images(
        &self,
        c: &Connection,
        position: &SourcePosition,
        references: &[Mutation],
        rows: usize,
        bytes: usize,
    ) -> Result<Vec<RetainedImage>> {
        ensure!(
            (1..=64).contains(&rows) && (1..=256 * 1024).contains(&bytes),
            "attention capture page bound"
        );
        ensure!(
            installer_metadata_unshadowed(c)?,
            "attention capture Installer metadata shadow"
        );
        ensure!(
            position.source == self.source
                && position.epoch == self.epoch
                && position.fingerprint == self.fingerprint,
            "attention capture identity changed"
        );
        ensure!(references.len() <= rows, "attention reference page bound");
        let mut metadata = Vec::new();
        let mut retained_bytes = 0usize;
        let mut unique = std::collections::BTreeSet::new();
        for reference in references {
            let key: serde_json::Value = serde_json::from_str(&reference.key)?;
            let table = key
                .get(0)
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("attention reference table absent"))?;
            ensure!(
                self.schema.iter().any(|(name, _)| name == table)
                    && !table.starts_with("local_attention_"),
                "attention reference table unsupported"
            );
            let mut revision = None;
            for (side, locator) in [(0, &reference.old), (1, &reference.new)] {
                let Some(locator) = locator else { continue };
                let number = locator
                    .get("revision")
                    .and_then(serde_json::Value::as_u64)
                    .ok_or_else(|| anyhow::anyhow!("attention reference revision absent"))?;
                ensure!(
                    number > 0
                        && number <= position.revision
                        && revision.is_none_or(|r| r == number)
                        && locator.get("table").and_then(serde_json::Value::as_str) == Some(table)
                        && locator.get("side").and_then(serde_json::Value::as_u64)
                            == Some(side as u64)
                        && unique.insert((number, side)),
                    "attention reference locator mismatch"
                );
                revision = Some(number);
                ensure!(metadata.len() < rows, "attention image side page bound");
                let (size, actual): (usize,usize) = c.query_row(
                    "SELECT bytes,length(CAST(body AS BLOB)) FROM main.local_attention_native_images WHERE source=?1 AND epoch=?2 AND revision=?3 AND side=?4 AND native_table=?5",
                    params![self.source,self.epoch,number,side,table], |r|Ok((r.get(0)?,r.get(1)?)),
                )?;
                ensure!(
                    size == actual && size <= MAX_IMAGE,
                    "attention immutable image byte mismatch"
                );
                retained_bytes = retained_bytes
                    .checked_add(size + table.len() + reference.key.len() + 32)
                    .ok_or_else(|| anyhow::anyhow!("attention image byte overflow"))?;
                ensure!(retained_bytes <= bytes, "attention image page byte bound");
                metadata.push((number, side, table.to_owned()));
            }
            ensure!(
                revision.is_some(),
                "attention reference has no mutation sides"
            );
        }
        metadata.into_iter().map(|(revision,side,table)| {
            let body = c.query_row(
                "SELECT body FROM main.local_attention_native_images WHERE source=?1 AND epoch=?2 AND revision=?3 AND side=?4 AND native_table=?5",
                params![self.source,self.epoch,revision,side,table], |r|r.get(0),
            )?;
            Ok(RetainedImage {revision,side,table,body})
        }).collect()
    }

    /// Capture only images whose references have already been reclaimed by
    /// the Installer. Its one journal remains the consumer retention authority.
    pub(crate) fn capture_reclaim(
        &self,
        c: &Connection,
        installer: &Installer,
        limit: usize,
    ) -> Result<ReclaimPage> {
        ensure!(
            installer_metadata_unshadowed(c)?,
            "attention reclaim Installer metadata shadow"
        );
        ensure!((1..=64).contains(&limit), "attention image reclaim bound");
        let position = installer.position(c, &self.source)?;
        ensure!(
            position.epoch == self.epoch && position.fingerprint == self.fingerprint,
            "attention image reclaim source changed"
        );
        // The retained journal is a contiguous suffix; a missing reference in
        // its interior is a source gap, never permission to reclaim that image.
        let first: Option<u64> = c.query_row(
            "SELECT MIN(revision) FROM main.ivm_install_journal WHERE source=?1",
            [&self.source],
            |r| r.get(0),
        )?;
        let through = first
            .map(|r| r.saturating_sub(1))
            .unwrap_or(position.revision);
        let (image_rows,image_bytes) = c.query_row("SELECT image_rows,image_bytes FROM main.local_attention_native_capture WHERE source=?1 AND epoch=?2",params![self.source,self.epoch],|r|Ok((r.get(0)?,r.get(1)?)))?;
        let entries = c.prepare_cached("SELECT revision,side,bytes FROM main.local_attention_native_images WHERE source=?1 AND epoch=?2 AND revision<=?3 ORDER BY revision,side LIMIT ?4")?
            .query_map(params![self.source,self.epoch,through,limit],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(ReclaimPage {
            position,
            image_rows,
            image_bytes,
            entries,
        })
    }

    /// Bounded precomputed maintenance on the background writer. Return false
    /// on a stale snapshot before deleting anything; no scan or reducer here.
    pub(crate) fn publish_reclaim(
        &self,
        tx: &Transaction<'_>,
        installer: &Installer,
        page: &ReclaimPage,
    ) -> Result<bool> {
        ensure!(
            installer_metadata_unshadowed(tx)?,
            "attention reclaim Installer metadata shadow"
        );
        if installer.position(tx, &self.source)? != page.position {
            return Ok(false);
        }
        let removed_bytes: usize = page.entries.iter().map(|entry| entry.2).sum();
        ensure!(
            removed_bytes <= page.image_bytes && page.entries.len() <= page.image_rows,
            "attention image reclaim accounting"
        );
        let changed = tx.execute("UPDATE main.local_attention_native_capture SET image_rows=?3,image_bytes=?4 WHERE source=?1 AND epoch=?2 AND image_rows=?5 AND image_bytes=?6",params![self.source,self.epoch,page.image_rows-page.entries.len(),page.image_bytes-removed_bytes,page.image_rows,page.image_bytes])?;
        if changed != 1 {
            return Ok(false);
        }
        ensure!(tx.execute("UPDATE main.local_attention_native_capture SET reclaiming=1 WHERE source=?1 AND epoch=?2 AND reclaiming=0", params![self.source,self.epoch])? == 1, "attention reclaim scope already open");
        for &(revision, side, size) in &page.entries {
            ensure!(tx.execute("DELETE FROM main.local_attention_native_images WHERE source=?1 AND epoch=?2 AND revision=?3 AND side=?4 AND bytes=?5",params![self.source,self.epoch,revision,side,size])? == 1,"attention retained image changed");
        }
        ensure!(tx.execute("UPDATE main.local_attention_native_capture SET reclaiming=0 WHERE source=?1 AND epoch=?2 AND reclaiming=1", params![self.source,self.epoch])? == 1, "attention reclaim scope missing");
        Ok(true)
    }
}
