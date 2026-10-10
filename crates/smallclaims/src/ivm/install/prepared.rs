//! Versioned deferred capture and off-writer preparation within the existing Installer.
//! Source owners retain immutable referenced input until every consumer has advanced. A
//! PreparedPage owns its inputs and metadata; it never retains a SQLite snapshot. Reducers
//! run after that snapshot is released. Publication executes only bounded, precomputed PK
//! writes and point checks, never Operator::apply or extraction/validation callbacks.
use super::*;
use rusqlite::{params_from_iter, types::Value as SqlValue};

pub(super) const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS ivm_install_deferred (
 source TEXT PRIMARY KEY, row_limit INTEGER NOT NULL, byte_limit INTEGER NOT NULL,
 reference_limit INTEGER NOT NULL, capturing INTEGER NOT NULL DEFAULT 0
);
CREATE TRIGGER IF NOT EXISTS ivm_deferred_reference_update AFTER UPDATE ON ivm_install_journal
WHEN EXISTS(SELECT 1 FROM ivm_install_deferred WHERE source=NEW.source OR source=OLD.source)
BEGIN
 UPDATE ivm_install_sources SET available=0 WHERE name IN (OLD.source,NEW.source);
 UPDATE ivm_install_roots SET ready=0,status_revision=status_revision+1,error='deferred reference modified' WHERE source IN (OLD.source,NEW.source);
 UPDATE ivm_install_jobs SET phase='stopped',error='deferred reference modified' WHERE source IN (OLD.source,NEW.source) AND phase IN ('scan','catchup');
END;
CREATE TRIGGER IF NOT EXISTS ivm_deferred_reference_insert AFTER INSERT ON ivm_install_journal
WHEN EXISTS(SELECT 1 FROM ivm_install_deferred d WHERE d.source=NEW.source AND
 (d.capturing<>1 OR NEW.revision<>(SELECT revision FROM ivm_install_sources WHERE name=NEW.source)))
BEGIN
 UPDATE ivm_install_sources SET available=0 WHERE name=NEW.source;
 UPDATE ivm_install_roots SET ready=0,status_revision=status_revision+1,error='unmanaged deferred reference' WHERE source=NEW.source;
 UPDATE ivm_install_jobs SET phase='stopped',error='unmanaged deferred reference' WHERE source=NEW.source AND phase IN ('scan','catchup');
END;
CREATE TRIGGER IF NOT EXISTS ivm_deferred_reference_delete AFTER DELETE ON ivm_install_journal
WHEN EXISTS(SELECT 1 FROM ivm_install_deferred WHERE source=OLD.source) AND OLD.revision>
 (SELECT COALESCE(MIN(revision),(SELECT revision FROM ivm_install_sources WHERE name=OLD.source)) FROM
  (SELECT applied_revision AS revision FROM ivm_install_jobs WHERE source=OLD.source AND phase IN ('scan','catchup')
   UNION ALL SELECT revision FROM ivm_install_roots WHERE source=OLD.source AND ready=1))
BEGIN
 UPDATE ivm_install_sources SET available=0 WHERE name=OLD.source;
 UPDATE ivm_install_roots SET ready=0,status_revision=status_revision+1,error='unconsumed deferred reference deleted' WHERE source=OLD.source;
 UPDATE ivm_install_jobs SET phase='stopped',error='unconsumed deferred reference deleted' WHERE source=OLD.source AND phase IN ('scan','catchup');
END;
"#;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CaptureLimits {
    pub rows: u64,
    pub bytes: u64,
    pub reference_bytes: usize,
}
impl CaptureLimits {
    fn validate(self) -> Result<()> {
        ensure!(
            (1..=4096).contains(&self.rows)
                && (1..=16 * 1024 * 1024).contains(&self.bytes)
                && (1..=4096).contains(&self.reference_bytes),
            "invalid deferred capture bound"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Capture {
    Queued(SourcePosition),
    Fenced(SourcePosition),
}

#[derive(Clone, Copy, Debug)]
pub struct PublicationLimits {
    pub rows: usize,
    pub bytes: usize,
    pub tables: usize,
}
impl Default for PublicationLimits {
    fn default() -> Self {
        Self {
            rows: 128,
            bytes: 1024 * 1024,
            tables: 32,
        }
    }
}
impl PublicationLimits {
    fn validate(self) -> Result<()> {
        ensure!(
            (1..=128).contains(&self.rows)
                && (1..=1024 * 1024).contains(&self.bytes)
                && (1..=32).contains(&self.tables),
            "invalid prepared publication bound"
        );
        Ok(())
    }
}

#[derive(Clone, Debug)]
struct Table {
    name: String,
    columns: Vec<String>,
    keys: Vec<String>,
}
// Private, one-call staging only: no schema authority or reusable proof. The image
// is consumed after the caller's final cookie check and is dropped on refusal.
struct TableImage {
    tables: BTreeMap<String, Table>,
}

#[derive(Clone, Debug)]
struct Write {
    sql: String,
    args: Vec<SqlValue>,
}
#[derive(Clone, Debug)]
struct Check {
    sql: String,
    args: Vec<SqlValue>,
    expected: Option<Vec<SqlValue>>,
    columns: usize,
}
#[derive(Clone, Debug)]
enum Target {
    Scan {
        id: String,
        cursor: Vec<u8>,
        next: Vec<u8>,
        finished: bool,
        base: u64,
    },
    CatchUp {
        id: String,
        applied: u64,
        base: u64,
    },
    Live {
        view: String,
        applied: u64,
        generation: u64,
        status: u64,
    },
}

/// Owned evidence captured in ONE short read snapshot. Capture required table metadata and
/// bounded namespace/source rows in that snapshot, then release it before computing writes.
/// SourcePosition is an identity/prefix, never authority or a graph-index certificate.
#[derive(Clone, Debug)]
pub struct PreparedPage {
    view: String,
    namespace: Namespace,
    position: SourcePosition,
    snapshot_position: SourcePosition,
    fingerprint: String,
    schema_version: i64,
    target: Target,
    rows: Vec<Mutation>,
    consumed_bytes: u64,
    limits: PublicationLimits,
    tables: BTreeMap<String, Table>,
    writes: Vec<Write>,
    checks: Vec<Check>,
    reads: Vec<Check>,
    output_bytes: usize,
    changed: bool,
}
impl PreparedPage {
    pub fn view(&self) -> &str {
        &self.view
    }
    pub fn namespace(&self) -> &Namespace {
        &self.namespace
    }
    /// Through the exact captured reference page; later admitted inputs remain pending.
    pub fn position(&self) -> &SourcePosition {
        &self.position
    }
    /// Source high-water observed in the preparation snapshot. This can be AHEAD of
    /// position(): mutable latest rows cannot substitute for the page's immutable versions.
    pub fn snapshot_position(&self) -> &SourcePosition {
        &self.snapshot_position
    }
    pub fn rows(&self) -> &[Mutation] {
        &self.rows
    }
    pub fn mark_visible_change(&mut self) {
        self.changed = true;
    }

    /// Call in the SAME read snapshot as prepare_*, before any off-writer computation.
    /// Ordinary namespace-keyed tables only. Generated/virtual tables are unsupported.
    pub fn capture_table(&mut self, db: &Connection, name: &str) -> Result<()> {
        self.capture_tables(db, &[name])
    }

    /// Capture a bounded group on the supplied preparation cut, sharing one foreign-key
    /// inventory traversal. Nothing is cached across calls/cuts; all metadata is accepted
    /// together only after every shape and fanout check succeeds. Duplicate group names refuse;
    /// the single-table wrapper may still recapture an existing table. Schema row caps do not
    /// qualify SQLite metadata VM/byte work; callers still need actual work accounting.
    pub fn capture_tables(&mut self, db: &Connection, names: &[&str]) -> Result<()> {
        ensure!(
            !names.is_empty() && names.len() <= self.limits.tables,
            "prepared table bound exceeded"
        );
        let requested = names
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>();
        ensure!(requested.len() == names.len(), "duplicate prepared table");
        for &name in names {
            identifier(name)?;
        }
        ensure!(
            self.tables
                .keys()
                .map(String::as_str)
                .chain(names.iter().copied())
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                <= self.limits.tables,
            "prepared table bound exceeded"
        );
        // The caller owns one preparation snapshot. Stage read-only metadata privately;
        // the final schema check below rejects a changed cut before accepting any table.
        // A second cookie read here does not strengthen that same-cut acceptance check.
        let image = self.capture_table_image(db, names)?;
        ensure!(
            schema_version(db)? == self.schema_version,
            "prepared table schema changed"
        );
        self.tables.extend(image.tables);
        Ok(())
    }

    fn capture_table_image(&self, db: &Connection, names: &[&str]) -> Result<TableImage> {
        let tables = self.capture_table_shapes(db, names)?;
        // The materialized inventory preserves the old 256-table refusal. A LEFT JOIN
        // emits a row even for a table with no foreign keys. Stream counts per table to
        // preserve the old 128 FK-column-row cap, including unrelated tables. The extra
        // global row detects truncation instead of treating an incomplete walk as safe.
        const SCHEMA_TABLES: usize = 256;
        const FK_ROWS: usize = 128;
        let mut statement = db.prepare(
            "WITH inventory AS MATERIALIZED (
               SELECT name FROM main.sqlite_schema WHERE type='table' ORDER BY name LIMIT 257
             )
             SELECT inventory.name,f.id,f.\"table\"
             FROM inventory LEFT JOIN pragma_foreign_key_list(inventory.name,'main') AS f ON 1
             LIMIT ?1",
        )?;
        let mut rows = statement.query([(SCHEMA_TABLES * FK_ROWS + 1) as i64])?;
        let mut inventory = BTreeMap::<String, usize>::new();
        let mut count = 0;
        while let Some(row) = rows.next()? {
            count += 1;
            ensure!(
                count <= SCHEMA_TABLES * FK_ROWS,
                "prepared foreign-key inventory exceeds bound"
            );
            let candidate: String = row.get(0)?;
            let fk_id: Option<i64> = row.get(1)?;
            let target: Option<String> = row.get(2)?;
            let foreign_keys = inventory.entry(candidate.clone()).or_default();
            match (fk_id, target) {
                (None, None) => {}
                (Some(_), Some(target)) => {
                    *foreign_keys += 1;
                    ensure!(
                        *foreign_keys <= FK_ROWS,
                        "prepared foreign-key inventory exceeds bound"
                    );
                    ensure!(
                        !names.iter().any(|name| candidate.eq_ignore_ascii_case(name)
                            || target.eq_ignore_ascii_case(name)),
                        "prepared output foreign keys unsupported"
                    );
                }
                _ => anyhow::bail!("prepared malformed foreign-key metadata"),
            }
            ensure!(
                inventory.len() <= SCHEMA_TABLES,
                "prepared schema inventory exceeds bound"
            );
        }
        Ok(TableImage { tables })
    }

    fn capture_table_shapes(
        &self,
        db: &Connection,
        names: &[&str],
    ) -> Result<BTreeMap<String, Table>> {
        // Names were validated and bounded before discovery. Materialize headers so
        // table_list and trigger discovery are shared, rather than repeated per field.
        // NOT INDEXED keeps the small bounded header join from building an autoindex.
        // The LEFT JOIN retains missing/zero-column shapes for explicit refusal.
        let requested = vec!["(?)"; names.len()].join(",");
        let sql = format!(
            "WITH requested(name) AS (VALUES {requested}),
             kinds AS MATERIALIZED (
               SELECT name,type FROM pragma_table_list WHERE schema='main'
                 AND name IN (SELECT name FROM requested)
             ), headers AS MATERIALIZED (
               SELECT requested.name,kinds.type,
                 EXISTS(SELECT 1 FROM main.sqlite_schema
                        WHERE type='trigger' AND tbl_name=requested.name) AS triggered
               FROM requested LEFT JOIN kinds NOT INDEXED ON kinds.name=requested.name
             )
             SELECT headers.name,headers.type,headers.triggered,f.name,f.pk,f.hidden
             FROM headers LEFT JOIN pragma_table_xinfo(headers.name,'main') AS f ON 1
             LIMIT {}",
            names.len() * 128 + 1
        );
        let mut statement = db.prepare(&sql)?;
        let mut rows = statement.query(params_from_iter(names.iter()))?;
        let mut fields = BTreeMap::<String, Vec<(String, u32)>>::new();
        let mut count = 0;
        while let Some(row) = rows.next()? {
            count += 1;
            ensure!(
                count <= names.len() * 128,
                "prepared generated/oversized table unsupported"
            );
            let name: String = row.get(0)?;
            let kind: Option<String> = row.get(1)?;
            ensure!(
                kind.as_deref() == Some("table"),
                "prepared virtual/shadow/missing table unsupported"
            );
            ensure!(
                !row.get::<_, bool>(2)?,
                "prepared output triggers unsupported"
            );
            let column: Option<String> = row.get(3)?;
            let pk: Option<u32> = row.get(4)?;
            let hidden: Option<u32> = row.get(5)?;
            let (Some(column), Some(pk), Some(0)) = (column, pk, hidden) else {
                anyhow::bail!("prepared generated/oversized table unsupported");
            };
            identifier(&column)?;
            let columns = fields.entry(name).or_default();
            ensure!(
                columns.len() < 128,
                "prepared generated/oversized table unsupported"
            );
            columns.push((column, pk));
        }
        ensure!(
            fields.len() == names.len(),
            "prepared table metadata incomplete"
        );
        let mut tables = BTreeMap::new();
        for &name in names {
            let fields = fields
                .get(name)
                .context("prepared table metadata incomplete")?;
            let columns = fields.iter().map(|f| f.0.clone()).collect::<Vec<_>>();
            let mut primary = fields.iter().filter(|f| f.1 > 0).collect::<Vec<_>>();
            primary.sort_by_key(|f| f.1);
            ensure!(
                primary.iter().any(|f| f.0 == "namespace"),
                "namespace must be part of prepared primary key"
            );
            let keys = primary
                .iter()
                .filter(|f| f.0 != "namespace")
                .map(|f| f.0.clone())
                .collect();
            tables.insert(
                name.to_owned(),
                Table {
                    name: name.into(),
                    columns,
                    keys,
                },
            );
        }
        Ok(tables)
    }
    /// Values cover all non-namespace columns in captured table order; namespace is injected.
    pub fn upsert(&mut self, table: &str, values: Vec<SqlValue>) -> Result<()> {
        let metadata = self
            .tables
            .get(table)
            .context("prepared table not captured")?;
        let columns = metadata
            .columns
            .iter()
            .filter(|c| *c != "namespace")
            .collect::<Vec<_>>();
        ensure!(
            values.len() == columns.len(),
            "prepared upsert column count mismatch"
        );
        for key in &metadata.keys {
            let i = columns
                .iter()
                .position(|c| *c == key)
                .context("prepared primary key missing")?;
            ensure!(
                !matches!(values[i], SqlValue::Null),
                "null prepared primary key unsupported"
            );
        }
        self.add_bytes(value_bytes(&values))?;
        ensure!(
            self.rows.len() + self.writes.len() + self.checks.len() + self.reads.len()
                < self.limits.rows,
            "prepared output row bound exceeded"
        );
        let metadata = self
            .tables
            .get(table)
            .context("prepared table not captured")?;
        let mut columns = vec!["namespace".into()];
        columns.extend(
            metadata
                .columns
                .iter()
                .filter(|c| *c != "namespace")
                .cloned(),
        );
        let mut keys = vec!["namespace".into()];
        keys.extend(metadata.keys.clone());
        let conflict = keys.iter().map(|c| quote(c)).collect::<Vec<_>>().join(",");
        let fields = columns
            .iter()
            .map(|c| quote(c))
            .collect::<Vec<_>>()
            .join(",");
        let update = columns
            .iter()
            .filter(|c| !keys.contains(c))
            .map(|c| format!("{}=excluded.{}", quote(c), quote(c)))
            .collect::<Vec<_>>()
            .join(",");
        let action = if update.is_empty() {
            "DO NOTHING".into()
        } else {
            format!("DO UPDATE SET {update}")
        };
        let sql = format!(
            "INSERT INTO {}({fields}) VALUES({}) ON CONFLICT({conflict}) {action}",
            quote(&metadata.name),
            vec!["?"; columns.len()].join(",")
        );
        let mut args = vec![SqlValue::Text(self.namespace.0.clone())];
        args.extend(values);
        ensure!(
            sql.len() + value_bytes(&args) <= 64 * 1024,
            "prepared statement row exceeds 64KiB"
        );
        self.add_bytes(sql.len() + self.namespace.0.len())?;
        self.writes.push(Write { sql, args });
        Ok(())
    }
    pub fn delete(&mut self, table: &str, keys: Vec<SqlValue>) -> Result<()> {
        let metadata = self
            .tables
            .get(table)
            .context("prepared table not captured")?;
        ensure!(
            keys.len() == metadata.keys.len() && keys.iter().all(|k| !matches!(k, SqlValue::Null)),
            "prepared delete key mismatch"
        );
        self.add_bytes(value_bytes(&keys))?;
        ensure!(
            self.rows.len() + self.writes.len() + self.checks.len() + self.reads.len()
                < self.limits.rows,
            "prepared output row bound exceeded"
        );
        let metadata = self
            .tables
            .get(table)
            .context("prepared table not captured")?;
        let mut fields = vec!["namespace".into()];
        fields.extend(metadata.keys.clone());
        let predicate = fields
            .iter()
            .map(|c| format!("{}=?", quote(c)))
            .collect::<Vec<_>>()
            .join(" AND ");
        let sql = format!("DELETE FROM {} WHERE {predicate}", quote(&metadata.name));
        let mut args = vec![SqlValue::Text(self.namespace.0.clone())];
        args.extend(keys);
        ensure!(
            sql.len() + value_bytes(&args) <= 64 * 1024,
            "prepared statement row exceeds 64KiB"
        );
        self.add_bytes(sql.len() + self.namespace.0.len())?;
        self.writes.push(Write { sql, args });
        Ok(())
    }
    /// Source/operator-owned completeness evidence checked by indexed PK after output writes.
    /// This is not inferred authority: its fields and source fingerprint need consumer review.
    /// Fresh publication requires at least one nonempty declared check.
    pub fn require_row(
        &mut self,
        table: &str,
        keys: Vec<SqlValue>,
        expected: Vec<(String, SqlValue)>,
    ) -> Result<()> {
        let metadata = self
            .tables
            .get(table)
            .context("prepared evidence table not captured")?;
        ensure!(
            keys.len() == metadata.keys.len()
                && keys.iter().all(|k| !matches!(k, SqlValue::Null))
                && !expected.is_empty()
                && expected.len() <= 128,
            "prepared evidence key/value mismatch"
        );
        ensure!(
            self.rows.len() + self.writes.len() + self.checks.len() + self.reads.len()
                < self.limits.rows
                && expected.iter().all(|(c, _)| metadata.columns.contains(c)),
            "prepared evidence bound/column mismatch"
        );
        self.add_bytes(
            value_bytes(&keys)
                + expected
                    .iter()
                    .map(|(c, v)| c.len() + value_bytes(std::slice::from_ref(v)))
                    .sum::<usize>(),
        )?;
        let metadata = self
            .tables
            .get(table)
            .context("prepared evidence table absent")?;
        let mut fields = vec!["namespace".into()];
        fields.extend(metadata.keys.clone());
        let predicate = fields
            .iter()
            .map(|c| format!("{}=?", quote(c)))
            .collect::<Vec<_>>()
            .join(" AND ");
        let columns = expected
            .iter()
            .map(|(c, _)| quote(c))
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT {columns} FROM {} WHERE {predicate}",
            quote(&metadata.name)
        );
        let mut args = vec![SqlValue::Text(self.namespace.0.clone())];
        args.extend(keys);
        ensure!(
            sql.len()
                + value_bytes(&args)
                + expected
                    .iter()
                    .map(|(_, v)| value_bytes(std::slice::from_ref(v)))
                    .sum::<usize>()
                <= 64 * 1024,
            "prepared evidence row exceeds 64KiB"
        );
        self.add_bytes(sql.len() + self.namespace.0.len())?;
        let count = expected.len();
        self.checks.push(Check {
            sql,
            args,
            expected: Some(expected.into_iter().map(|(_, v)| v).collect()),
            columns: count,
        });
        Ok(())
    }
    /// Capture a namespace point read (including absence) in the same preparation snapshot.
    /// Columns must include the owner's complete version/evidence dependencies. The values
    /// are returned owned; publication checks them BEFORE writes, and never re-runs reducers.
    pub fn observe_row(
        &mut self,
        db: &Connection,
        table: &str,
        keys: Vec<SqlValue>,
        columns: Vec<String>,
    ) -> Result<Option<Vec<SqlValue>>> {
        ensure!(
            schema_version(db)? == self.schema_version,
            "prepared read schema changed"
        );
        let metadata = self
            .tables
            .get(table)
            .context("prepared read table not captured")?;
        ensure!(
            keys.len() == metadata.keys.len()
                && keys.iter().all(|k| !matches!(k, SqlValue::Null))
                && !columns.is_empty()
                && columns.len() <= 128
                && columns.iter().all(|c| metadata.columns.contains(c)),
            "prepared read key/column mismatch"
        );
        ensure!(
            self.rows.len() + self.writes.len() + self.checks.len() + self.reads.len()
                < self.limits.rows,
            "prepared aggregate point/write bound exceeded"
        );
        let mut fields = vec!["namespace".into()];
        fields.extend(metadata.keys.clone());
        let predicate = fields
            .iter()
            .map(|c| format!("{}=?", quote(c)))
            .collect::<Vec<_>>()
            .join(" AND ");
        let sql = format!(
            "SELECT {} FROM {} WHERE {predicate}",
            columns
                .iter()
                .map(|c| quote(c))
                .collect::<Vec<_>>()
                .join(","),
            quote(&metadata.name)
        );
        let mut args = vec![SqlValue::Text(self.namespace.0.clone())];
        args.extend(keys);
        let expected: Option<Vec<SqlValue>> = db
            .query_row(&sql, params_from_iter(&args), |r| {
                bounded_values(r, columns.len())
            })
            .optional()?;
        let bytes =
            sql.len() + value_bytes(&args) + expected.as_ref().map_or(0, |v| value_bytes(v));
        ensure!(bytes <= 64 * 1024, "prepared observed row exceeds 64KiB");
        self.add_bytes(bytes)?;
        self.reads.push(Check {
            sql,
            args,
            expected: expected.clone(),
            columns: columns.len(),
        });
        Ok(expected)
    }
    fn add_bytes(&mut self, bytes: usize) -> Result<()> {
        ensure!(bytes <= 64 * 1024, "prepared row exceeds 64KiB");
        let next = self
            .output_bytes
            .checked_add(bytes)
            .context("prepared output size overflow")?;
        ensure!(
            next <= self.limits.bytes,
            "prepared output byte bound exceeded"
        );
        self.output_bytes = next;
        Ok(())
    }
}

impl Installer {
    pub(super) fn deferred(&self, db: &Connection, source: &str) -> Result<bool> {
        Ok(db.query_row(
            "SELECT EXISTS(SELECT 1 FROM ivm_install_deferred WHERE source=?1)",
            [source],
            |r| r.get(0),
        )?)
    }
    /// Explicit new source lifetime only. No migration or default registration; flag-off calls
    /// none of these methods. Mutation values must be durable immutable references, not a
    /// later lookup of mutable current rows. Their retention/extraction is source-owner policy.
    pub fn enable_deferred(
        &self,
        tx: &Transaction<'_>,
        expected: &SourcePosition,
        limits: CaptureLimits,
    ) -> Result<()> {
        limits.validate()?;
        ensure!(
            self.operators.len() <= 256,
            "deferred registry exceeds 256 operators"
        );
        ensure!(
            self.position(tx, &expected.source)? == *expected && expected.revision == 0,
            "deferred mode requires fresh source lifetime"
        );
        ensure!(
            !tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM ivm_install_jobs WHERE source=?1)",
                [&expected.source],
                |r| r.get::<_, bool>(0)
            )?,
            "enable deferred mode before starting jobs"
        );
        tx.execute("INSERT INTO ivm_install_deferred(source,row_limit,byte_limit,reference_limit) VALUES(?1,?2,?3,?4)",params![expected.source,limits.rows,limits.bytes,limits.reference_bytes])?;
        Ok(())
    }
    /// Only queue/reference work in source admission. Never calls an Operator. Quota/invalid
    /// reference fences derived state but preserves valid raw admission; SQLite errors propagate.
    pub fn capture_deferred(
        &self,
        tx: &Transaction<'_>,
        source: &str,
        reference: &Mutation,
    ) -> Result<Capture> {
        let (mut position, available) = source_position(tx, source)?;
        let limits: CaptureLimits = tx.query_row(
            "SELECT row_limit,byte_limit,reference_limit FROM ivm_install_deferred WHERE source=?1",
            [source],
            |r| {
                Ok(CaptureLimits {
                    rows: r.get(0)?,
                    bytes: r.get(1)?,
                    reference_bytes: r.get(2)?,
                })
            },
        )?;
        position.revision = position
            .revision
            .checked_add(1)
            .filter(|r| *r <= i64::MAX as u64)
            .context("deferred revision overflow")?;
        tx.execute(
            "UPDATE ivm_install_sources SET revision=?2 WHERE name=?1",
            params![source, position.revision],
        )?;
        if !available {
            return Ok(Capture::Fenced(position));
        }
        if !bounded_reference(reference, limits.reference_bytes) {
            self.source_gap(
                tx,
                source,
                "deferred reference exceeds bound; explicit recovery required",
            )?;
            return Ok(Capture::Fenced(position));
        }
        let payload = serde_json::to_string(reference)?;
        let bytes = payload.len() as u64;
        let (rows, retained): (u64, u64) = tx.query_row(
            "SELECT journal_rows,journal_bytes FROM ivm_install_sources WHERE name=?1",
            [source],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if reference.key.is_empty()
            || reference.key.len() > 1024
            || bytes > limits.reference_bytes as u64
            || rows >= limits.rows
            || bytes > limits.bytes.saturating_sub(retained)
        {
            self.source_gap(
                tx,
                source,
                "deferred reference invalid or queue exhausted; explicit recovery required",
            )?;
            return Ok(Capture::Fenced(position));
        }
        tx.execute(
            "UPDATE ivm_install_deferred SET capturing=1 WHERE source=?1",
            [source],
        )?;
        tx.execute(
            "INSERT INTO ivm_install_journal VALUES(?1,?2,?3,?4)",
            params![source, position.revision, payload, bytes],
        )?;
        tx.execute(
            "UPDATE ivm_install_deferred SET capturing=0 WHERE source=?1",
            [source],
        )?;
        tx.execute("UPDATE ivm_install_sources SET journal_rows=journal_rows+1,journal_bytes=journal_bytes+?2 WHERE name=?1",params![source,bytes])?;
        tx.execute("UPDATE ivm_install_jobs SET queued_rows=queued_rows+1,queued_bytes=queued_bytes+?2 WHERE source=?1 AND phase IN ('scan','catchup')",params![source,bytes])?;
        Ok(Capture::Queued(position))
    }
    fn prepared_page(
        &self,
        db: &Connection,
        source: &str,
        namespace: Namespace,
        fingerprint: String,
        target: Target,
        limits: PublicationLimits,
    ) -> Result<PreparedPage> {
        limits.validate()?;
        ensure!(self.deferred(db, source)?, "source is not deferred");
        let view = match &target {
            Target::Scan { id, .. } | Target::CatchUp { id, .. } => load_job(db, id)?.progress.view,
            Target::Live { view, .. } => view.clone(),
        };
        let position = self.position(db, source)?;
        Ok(PreparedPage {
            snapshot_position: position.clone(),
            view,
            namespace,
            position,
            fingerprint,
            schema_version: schema_version(db)?,
            target,
            rows: vec![],
            consumed_bytes: 0,
            limits,
            tables: BTreeMap::new(),
            writes: vec![],
            checks: vec![],
            reads: vec![],
            output_bytes: 0,
            changed: false,
        })
    }
    pub fn prepare_scan(
        &self,
        db: &Connection,
        scan: &ScanPage,
        limits: PublicationLimits,
    ) -> Result<PreparedPage> {
        let job = load_job(db, &scan.job)?;
        ensure!(
            job.progress.phase == "scan" && job.progress.cursor == scan.expected_cursor,
            "prepared scan cursor mismatch"
        );
        let mut page = self.prepared_page(
            db,
            &job.source,
            Namespace(scan.job.clone()),
            job.fingerprint.clone(),
            Target::Scan {
                id: scan.job.clone(),
                cursor: scan.expected_cursor.clone(),
                next: scan.next_cursor.clone(),
                finished: scan.finished,
                base: job.progress.base_revision,
            },
            limits,
        )?;
        ensure!(
            scan.position.source == page.position.source
                && scan.position.fingerprint == page.position.fingerprint
                && scan.position.epoch == page.position.epoch
                && scan.position.revision == page.position.revision
                && scan.position.revision >= job.progress.base_revision,
            "prepared scan source mismatch"
        );
        ensure!(
            scan.next_cursor > scan.expected_cursor
                || (scan.finished
                    && scan.rows.is_empty()
                    && scan.next_cursor == scan.expected_cursor),
            "prepared scan made no progress"
        );
        ensure!(
            scan.rows.iter().all(|m| m.old.is_none() && m.new.is_some()),
            "prepared scan requires current replacements"
        );
        validate_page(&job.limits, &scan.rows)?;
        validate_total_work(&job, scan.rows.len())?;
        ensure!(
            scan.rows.len() <= limits.rows && serde_json::to_vec(&scan.rows)?.len() <= limits.bytes,
            "prepared input page exceeds publication bound"
        );
        ensure!(
            scan.rows
                .iter()
                .all(|r| serde_json::to_vec(r).is_ok_and(|v| v.len() <= 64 * 1024)),
            "prepared scan row exceeds 64KiB"
        );
        page.output_bytes = serde_json::to_vec(&scan.rows)?.len();
        page.rows = scan.rows.clone();
        Ok(page)
    }
    pub fn prepare_catch_up(
        &self,
        db: &Connection,
        id: &str,
        limits: PublicationLimits,
    ) -> Result<PreparedPage> {
        let job = load_job(db, id)?;
        ensure!(
            job.progress.phase == "catchup",
            "prepared extraction incomplete"
        );
        let mut page = self.prepared_page(
            db,
            &job.source,
            Namespace(id.into()),
            job.fingerprint.clone(),
            Target::CatchUp {
                id: id.into(),
                applied: job.progress.applied_revision,
                base: job.progress.base_revision,
            },
            limits,
        )?;
        page.load_references(
            db,
            job.progress.applied_revision,
            job.limits.page_rows.min(limits.rows),
            job.limits.page_bytes.min(limits.bytes),
        )?;
        validate_total_work(&job, page.rows.len())?;
        Ok(page)
    }
    pub fn prepare_live(
        &self,
        db: &Connection,
        view: &str,
        limits: PublicationLimits,
    ) -> Result<PreparedPage> {
        self.prepare_live_inner(db, view, limits, limits.rows, limits.bytes)
    }
    /// Bound input references separately from the combined input/write/evidence budget.
    /// This allows a page to consume N inputs and publish their bounded outputs without
    /// selecting extra inputs merely to leave room for writes. Both budgets are cumulative.
    pub fn prepare_live_bounded(
        &self,
        db: &Connection,
        view: &str,
        limits: PublicationLimits,
        input_rows: usize,
        input_bytes: usize,
    ) -> Result<PreparedPage> {
        limits.validate()?;
        ensure!(
            (1..=limits.rows).contains(&input_rows) && (1..=limits.bytes).contains(&input_bytes),
            "prepared input bound exceeds publication budget"
        );
        self.prepare_live_inner(db, view, limits, input_rows, input_bytes)
    }
    fn prepare_live_inner(
        &self,
        db: &Connection,
        view: &str,
        limits: PublicationLimits,
        input_rows: usize,
        input_bytes: usize,
    ) -> Result<PreparedPage> {
        let (namespace,source,fingerprint,source_fp,epoch,applied,ready,generation,status):(String,String,String,String,u64,u64,bool,u64,u64)=db.query_row("SELECT namespace,source,fingerprint,source_fingerprint,epoch,revision,ready,generation,status_revision FROM ivm_install_roots WHERE view=?1",[view],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?,r.get(8)?)))?;
        ensure!(
            ready && self.fingerprints.get(view) == Some(&fingerprint),
            "prepared live root fenced or incompatible"
        );
        let mut page = self.prepared_page(
            db,
            &source,
            Namespace(namespace),
            fingerprint,
            Target::Live {
                view: view.into(),
                applied,
                generation,
                status,
            },
            limits,
        )?;
        ensure!(
            page.position.epoch == epoch && page.position.fingerprint == source_fp,
            "prepared live source replaced"
        );
        page.load_references(db, applied, input_rows, input_bytes)?;
        Ok(page)
    }
    /// Apply only precomputed writes. Caller uses a short managed reactor transaction and
    /// yields between calls. Failure rolls back output AND progress; no source admission is
    /// part of this transaction. New source writes may remain queued beyond this page.
    pub fn publish_prepared(
        &self,
        tx: &Transaction<'_>,
        page: &PreparedPage,
        now_ms: u64,
    ) -> Result<Outcome> {
        tx.execute_batch("SAVEPOINT ivm_prepared_page")?;
        let result = self.publish_prepared_inner(tx, page, now_ms);
        match result {
            Ok(outcome) => {
                tx.execute_batch("RELEASE ivm_prepared_page")?;
                Ok(outcome)
            }
            Err(error) => {
                tx.execute_batch("ROLLBACK TO ivm_prepared_page; RELEASE ivm_prepared_page")?;
                Err(error)
            }
        }
    }
    fn publish_prepared_inner(
        &self,
        tx: &Transaction<'_>,
        page: &PreparedPage,
        now_ms: u64,
    ) -> Result<Outcome> {
        ensure!(
            schema_version(tx)? == page.schema_version,
            "prepared publication schema changed"
        );
        let current = self.position(tx, &page.position.source)?;
        ensure!(
            current.fingerprint == page.position.fingerprint
                && current.epoch == page.position.epoch
                && current.revision >= page.position.revision,
            "prepared source identity/prefix changed"
        );
        match &page.target {
            Target::Scan { id, base, .. } | Target::CatchUp { id, base, .. } => {
                let job = load_job(tx, id)?;
                self.check(tx, &job, now_ms)?;
                ensure!(
                    job.fingerprint == page.fingerprint && job.progress.base_revision == *base,
                    "prepared job replaced"
                );
                match &page.target {
                    Target::Scan { cursor, .. } => ensure!(
                        job.progress.phase == "scan" && job.progress.cursor == *cursor,
                        "prepared scan superseded"
                    ),
                    Target::CatchUp { applied, .. } => ensure!(
                        job.progress.phase == "catchup"
                            && job.progress.applied_revision == *applied,
                        "prepared catch-up superseded"
                    ),
                    _ => unreachable!(),
                }
            }
            Target::Live {
                view,
                applied,
                generation,
                status,
            } => {
                let actual:(String,String,u64,u64,u64,bool)=tx.query_row("SELECT namespace,fingerprint,revision,generation,status_revision,ready FROM ivm_install_roots WHERE view=?1",[view],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?)))?;
                ensure!(
                    actual
                        == (
                            page.namespace.0.clone(),
                            page.fingerprint.clone(),
                            *applied,
                            *generation,
                            *status,
                            true
                        )
                        && self.fingerprints.get(view.as_str()) == Some(&page.fingerprint),
                    "prepared live root changed or fenced"
                );
            }
        }
        for check in &page.reads {
            page.check_row(tx, check)?;
        }
        for write in &page.writes {
            page.execute_write(tx, write)?;
        }
        for check in &page.checks {
            page.check_row(tx, check)?;
        }
        match &page.target {
            Target::Scan {
                id, next, finished, ..
            } => {
                tx.execute("UPDATE ivm_install_jobs SET cursor=?2,phase=?3,pages=pages+1,extracted_rows=extracted_rows+?4 WHERE id=?1",params![id,next,if *finished {"catchup"}else{"scan"},page.rows.len()])?;
                Ok(Outcome::Progress)
            }
            Target::CatchUp { id, .. } => {
                tx.execute("UPDATE ivm_install_jobs SET applied_revision=?2,queued_rows=queued_rows-?3,queued_bytes=queued_bytes-?4,pages=pages+1,applied_rows=applied_rows+?3 WHERE id=?1",params![id,page.position.revision,page.rows.len(),page.consumed_bytes])?;
                if page.position.revision != current.revision {
                    return Ok(Outcome::Progress);
                }
                ensure!(
                    !page.checks.is_empty(),
                    "prepared publication requires declared completeness evidence"
                );
                let job = load_job(tx, id)?;
                ensure!(
                    job.progress.queued_rows == 0 && job.progress.queued_bytes == 0,
                    "prepared journal counters disagree"
                );
                tx.execute("UPDATE ivm_install_roots SET namespace=?2,revision=?3,ready=1,generation=generation+1,status_revision=status_revision+1,error=NULL WHERE view=?1",params![job.progress.view,id,current.revision])?;
                tx.execute("UPDATE ivm_install_jobs SET phase='done' WHERE id=?1", [id])?;
                Ok(Outcome::Published)
            }
            Target::Live { view, .. } => {
                tx.execute("UPDATE ivm_install_roots SET revision=?2,generation=generation+?3 WHERE view=?1",params![view,page.position.revision,page.changed])?;
                Ok(Outcome::Progress)
            }
        }
    }
}

impl PreparedPage {
    fn load_references(
        &mut self,
        db: &Connection,
        mut through: u64,
        row_limit: usize,
        byte_limit: usize,
    ) -> Result<()> {
        let current = self.position.revision;
        ensure!(through <= current, "prepared prefix ahead of source");
        let mut statement=db.prepare_cached("SELECT revision,payload,bytes FROM ivm_install_journal WHERE source=?1 AND revision>?2 AND revision<=?3 ORDER BY revision LIMIT ?4")?;
        let mut rows =
            statement.query(params![self.position.source, through, current, row_limit])?;
        while let Some(row) = rows.next()? {
            let revision: u64 = row.get(0)?;
            let bytes: u64 = row.get(2)?;
            ensure!(revision == through + 1, "prepared reference journal gap");
            if self.consumed_bytes + bytes > byte_limit as u64 {
                break;
            }
            ensure!(bytes <= 4096, "prepared reference exceeds 4KiB");
            let raw = row.get_ref(1)?;
            ensure!(
                matches!(raw,rusqlite::types::ValueRef::Text(v) if v.len()<=4096),
                "prepared reference payload exceeds bound"
            );
            let payload: String = row.get(1)?;
            ensure!(
                payload.len() as u64 == bytes,
                "prepared reference byte mismatch"
            );
            self.add_bytes(payload.len())?;
            self.rows.push(serde_json::from_str(&payload)?);
            self.consumed_bytes += bytes;
            through = revision;
        }
        ensure!(
            through == current || !self.rows.is_empty(),
            "prepared reference gap or oversized input"
        );
        self.position.revision = through;
        Ok(())
    }
    fn execute_write(&self, tx: &Transaction<'_>, write: &Write) -> Result<()> {
        tx.execute(&write.sql, params_from_iter(&write.args))?;
        Ok(())
    }
    fn check_row(&self, tx: &Transaction<'_>, check: &Check) -> Result<()> {
        let actual: Option<Vec<SqlValue>> = tx
            .query_row(&check.sql, params_from_iter(&check.args), |r| {
                bounded_values(r, check.columns)
            })
            .optional()?;
        ensure!(
            actual == check.expected,
            "prepared completeness evidence unavailable"
        );
        Ok(())
    }
}
fn identifier(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name.len() <= 128
            && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
            && !name.as_bytes()[0].is_ascii_digit(),
        "invalid prepared SQL identifier"
    );
    Ok(())
}
fn quote(name: &str) -> String {
    format!("\"{name}\"")
}
fn schema_version(db: &Connection) -> Result<i64> {
    Ok(db.query_row("PRAGMA main.schema_version", [], |r| r.get(0))?)
}
fn value_bytes(values: &[SqlValue]) -> usize {
    values
        .iter()
        .map(|v| match v {
            SqlValue::Null => 1,
            SqlValue::Integer(_) | SqlValue::Real(_) => 8,
            SqlValue::Text(s) => s.len(),
            SqlValue::Blob(b) => b.len(),
        })
        .sum()
}

fn bounded_values(row: &rusqlite::Row<'_>, columns: usize) -> rusqlite::Result<Vec<SqlValue>> {
    let mut bytes = 0usize;
    for i in 0..columns {
        bytes = bytes.saturating_add(match row.get_ref(i)? {
            rusqlite::types::ValueRef::Text(v) | rusqlite::types::ValueRef::Blob(v) => v.len(),
            _ => 8,
        });
        if bytes > 64 * 1024 {
            return Err(rusqlite::Error::InvalidQuery);
        }
    }
    (0..columns).map(|i| row.get(i)).collect()
}

// Check before serializing: no body-sized allocation/work on admission even if a
// caller accidentally supplies an unbounded Value instead of a small immutable locator.
fn bounded_reference(reference: &Mutation, limit: usize) -> bool {
    let mut bytes = reference.key.len();
    let mut nodes = 0usize;
    let mut pending = Vec::new();
    if let Some(value) = &reference.old {
        pending.push(value)
    }
    if let Some(value) = &reference.new {
        pending.push(value)
    }
    while let Some(value) = pending.pop() {
        nodes += 1;
        if nodes > 128 || bytes > limit {
            return false;
        }
        bytes = bytes.saturating_add(16);
        match value {
            serde_json::Value::String(s) => bytes = bytes.saturating_add(s.len()),
            serde_json::Value::Array(items) => {
                if items.len() + nodes + pending.len() > 128 {
                    return false;
                }
                pending.extend(items.iter());
            }
            serde_json::Value::Object(items) => {
                if items.len() + nodes + pending.len() > 128 {
                    return false;
                }
                for (key, value) in items {
                    bytes = bytes.saturating_add(key.len());
                    pending.push(value)
                }
            }
            _ => {}
        }
    }
    bytes <= limit
}
