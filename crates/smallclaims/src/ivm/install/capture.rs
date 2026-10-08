//! Compiled, literal-only AFTER-trigger fragments for the existing deferred journal.
//! No second queue or revision counter is created. Source owners retain immutable images
//! at the appended revision, install complete trigger coverage and fence uncovered writes.
use super::*;

/// A replacement uses the same primary key. Rekeying is Delete followed by Insert, with
/// the corresponding immutable image saved immediately after EACH append fragment.
#[derive(Clone, Copy, Debug)]
pub enum Change {
    Insert,
    Delete,
    Replacement,
}

/// Owned SQL compiled during explicit source setup, never from caller SQL expressions.
/// Embed `append_sql` in an ordinary main-schema AFTER trigger, before retaining images.
/// The fragments share the Installer's only revision authority and mutation journal.
#[derive(Clone, Debug)]
pub struct CapturePlan {
    insert: String,
    delete: String,
    replacement: String,
    revision: String,
    available: String,
    gap: String,
    main_gap: String,
}
impl CapturePlan {
    pub fn append_sql(&self, change: Change) -> &str {
        match change {
            Change::Insert => &self.insert,
            Change::Delete => &self.delete,
            Change::Replacement => &self.replacement,
        }
    }
    /// Scalar SELECT expression for native immutable-image revision keys. Read only.
    pub fn revision_sql(&self) -> &str {
        &self.revision
    }
    /// Scalar boolean expression: only retain images while this exact source is available.
    pub fn available_sql(&self) -> &str {
        &self.available
    }
    /// Main-trigger fragment fencing this source, roots and jobs for a coverage gap.
    /// Its DML targets must be unqualified under SQLite's trigger syntax. Embed only in
    /// an ordinary main-schema trigger; use `gap_main_sql` outside a trigger.
    /// No revision append or Ready restoration. This literal reason is deliberately bounded.
    pub fn gap_sql(&self) -> &str {
        &self.gap
    }
    /// Main-qualified coverage fence for execution outside a trigger, in the source transaction.
    pub fn gap_main_sql(&self) -> &str {
        &self.main_gap
    }
}

impl Installer {
    /// Compile at explicit setup against a registered, enabled deferred source. `key` must
    /// be the ENTIRE ordinary table primary key in its declared order (1..=16 columns).
    /// DDL/schema/restore, raw scope, replacement conflict and image retention coverage are
    /// source-owner contracts, not inferred from this descriptor. No triggers are installed.
    pub fn capture_plan(
        &self,
        db: &Connection,
        expected: &SourcePosition,
        table: &str,
        key: &[&str],
    ) -> Result<CapturePlan> {
        ensure!(
            self.operators.len() <= 256,
            "capture registry exceeds bound"
        );
        ensure!(
            expected.source.len() <= 4096
                && expected.fingerprint.len() <= 4096
                && !expected.source.contains('\0')
                && !expected.fingerprint.contains('\0'),
            "capture source identity exceeds bound"
        );
        let current: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM main.ivm_install_sources WHERE name=?1 AND fingerprint=?2 AND epoch=?3 AND revision=?4 AND available=1)",
            rusqlite::params![expected.source, expected.fingerprint, expected.epoch, expected.revision],
            |r| r.get(0),
        )?;
        ensure!(current, "capture source changed");
        identifier(table)?;
        ensure!(
            !table.starts_with("ivm_")
                && !table.starts_with("st3_ivm_")
                && !table.starts_with("sqlite_")
                && (1..=16).contains(&key.len()),
            "invalid capture source table/key"
        );
        for column in key {
            identifier(column)?;
        }
        let kind: String = db.query_row(
            "SELECT type FROM pragma_table_list WHERE schema='main' AND name=?1",
            [table],
            |r| r.get(0),
        )?;
        ensure!(kind == "table", "capture virtual/shadow table unsupported");
        let mut statement =
            db.prepare("SELECT name,pk,hidden FROM pragma_table_xinfo(?1,'main') LIMIT 129")?;
        let fields = statement
            .query_map([table], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, usize>(1)?,
                    r.get::<_, usize>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ensure!(
            !fields.is_empty() && fields.len() <= 128 && fields.iter().all(|f| f.2 == 0),
            "capture generated/oversized source table unsupported"
        );
        let mut primary = fields.iter().filter(|f| f.1 > 0).collect::<Vec<_>>();
        primary.sort_by_key(|f| f.1);
        ensure!(
            primary.iter().map(|f| f.0.as_str()).eq(key.iter().copied()),
            "capture requires full ordered primary key"
        );
        let limits = db.query_row(
            "SELECT row_limit,byte_limit,reference_limit,capturing FROM main.ivm_install_deferred WHERE source=?1",
            [&expected.source], |r| Ok((r.get::<_,u64>(0)?, r.get::<_,u64>(1)?,
                r.get::<_,usize>(2)?, r.get::<_,i64>(3)?)))?;
        ensure!(
            (1..=4096).contains(&limits.0)
                && (1..=16 * 1024 * 1024).contains(&limits.1)
                && (1..=4096).contains(&limits.2)
                && limits.3 == 0,
            "invalid capture mode/bounds"
        );
        let source = literal(&expected.source);
        let identity = format!(
            "name={source} AND fingerprint={} AND epoch={}",
            literal(&expected.fingerprint),
            expected.epoch
        );
        let available = format!(
            "(SELECT COALESCE(MAX(available=1),0) FROM main.ivm_install_sources WHERE {identity})"
        );
        let revision = format!("(SELECT revision FROM main.ivm_install_sources WHERE {identity})");
        let gap = fence(&source, "");
        let main_gap = fence(&source, "main.");
        let compile = |change| append(table, key, change, &source, &identity, limits);
        let plan = CapturePlan {
            insert: compile(Change::Insert),
            delete: compile(Change::Delete),
            replacement: compile(Change::Replacement),
            available,
            revision,
            gap,
            main_gap,
        };
        ensure!(
            [&plan.insert, &plan.delete, &plan.replacement]
                .iter()
                .all(|s| s.len() <= 256 * 1024),
            "capture SQL plan exceeds bound"
        );
        Ok(plan)
    }
}

fn identifier(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name.len() <= 128
            && !name.as_bytes()[0].is_ascii_digit()
            && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'),
        "invalid capture SQL identifier"
    );
    Ok(())
}
fn literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}
fn column(prefix: &str, name: &str) -> String {
    format!("{prefix}.\"{name}\"")
}

// CASE short-circuits BEFORE hex/JSON rendering; even an enormous native key is bounded
// without extracting its body. TEXT lengths are bytes, not Unicode scalar counts.
fn bounded_key(table: &str, key: &[&str], prefix: &str) -> String {
    let valid = key
        .iter()
        .map(|c| {
            let c = column(prefix, c);
            format!(
                "({c} IS NOT NULL AND (typeof({c})<>'real' OR abs({c})<=1.7976931348623157e308))"
            )
        })
        .collect::<Vec<_>>()
        .join(" AND ");
    let bytes = key.iter().map(|c| {
        let c = column(prefix,c);
        format!("CASE typeof({c}) WHEN 'blob' THEN 2*length({c})+16 WHEN 'text' THEN length(CAST({c} AS BLOB)) ELSE 32 END")
    }).collect::<Vec<_>>().join("+");
    let values = key
        .iter()
        .map(|c| {
            let c = column(prefix, c);
            format!("CASE WHEN typeof({c})='blob' THEN json_object('$blob',hex({c})) ELSE {c} END")
        })
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "CASE WHEN {valid} AND ({bytes})<=1024 THEN json_array({},json_array({values})) ELSE NULL END",
        literal(table)
    )
}
fn payload(table: &str, key: &[&str], change: Change, revision: &str) -> String {
    let prefix = match change {
        Change::Delete => "OLD",
        _ => "NEW",
    };
    let key_sql = bounded_key(table, key, prefix);
    let locator = |side| {
        format!(
            "json_object('table',{},'revision',{revision},'side',{side})",
            literal(table)
        )
    };
    let old = match change {
        Change::Insert => "NULL".into(),
        _ => locator(0),
    };
    let new = match change {
        Change::Delete => "NULL".into(),
        _ => locator(1),
    };
    // Same-key replacement is mandatory: silently rekeying would erase old dependencies.
    let same = match change {
        Change::Replacement => key
            .iter()
            .map(|c| {
                format!(
                    "{} COLLATE BINARY IS {} COLLATE BINARY AND typeof({})=typeof({})",
                    column("OLD", c),
                    column("NEW", c),
                    column("OLD", c),
                    column("NEW", c)
                )
            })
            .collect::<Vec<_>>()
            .join(" AND "),
        _ => "1".into(),
    };
    // Strip SQLite's JSON subtype: Mutation.key is JSON text inside a string,
    // unlike the structured locator objects in old/new.
    format!(
        "CASE WHEN {same} AND ({key_sql}) IS NOT NULL AND length(CAST(({key_sql}) AS BLOB))<=1024 THEN json_object('key',({key_sql})||'','old',{old},'new',{new}) ELSE NULL END"
    )
}
fn fence(source: &str, schema: &str) -> String {
    let reason = "deferred trigger capture gap; explicit recovery required";
    format!(
        "UPDATE {schema}ivm_install_sources SET available=0 WHERE name={source};\n\
        UPDATE {schema}ivm_install_roots SET ready=0,status_revision=status_revision+1,error='{reason}' \
        WHERE source={source} AND (ready<>0 OR error IS NOT '{reason}');\n\
        UPDATE {schema}ivm_install_jobs SET phase='stopped',error='{reason}' WHERE source={source} AND phase IN ('scan','catchup');\n"
    )
}
fn append(
    table: &str,
    key: &[&str],
    change: Change,
    source: &str,
    identity: &str,
    limits: (u64, u64, usize, i64),
) -> String {
    let next = payload(table, key, change, "revision+1");
    let current = payload(table, key, change, "revision");
    let config = format!(
        "EXISTS(SELECT 1 FROM main.ivm_install_deferred WHERE source={source} AND row_limit={} AND byte_limit={} AND reference_limit={} AND capturing=0)",
        limits.0, limits.1, limits.2
    );
    let reason = "deferred trigger capture gap; explicit recovery required";
    format!(
        "UPDATE ivm_install_sources SET \
      available=CASE WHEN available=1 AND ({identity}) AND revision>=0 AND revision<9223372036854775807 \
        AND {config} AND journal_rows>=0 AND journal_rows<{} AND journal_bytes>=0 \
        AND ({next}) IS NOT NULL AND length(CAST(({next}) AS BLOB))<= {} \
        AND length(CAST(({next}) AS BLOB))<= {}-journal_bytes THEN 1 ELSE 0 END,\
      revision=CASE WHEN revision<9223372036854775807 THEN revision+1 ELSE revision END WHERE name={source};\n\
    UPDATE ivm_install_deferred SET capturing=1 WHERE source={source} AND EXISTS(SELECT 1 FROM main.ivm_install_sources WHERE {identity} AND available=1);\n\
    INSERT INTO ivm_install_journal(source,revision,payload,bytes) SELECT name,revision,({current}),length(CAST(({current}) AS BLOB)) FROM main.ivm_install_sources WHERE {identity} AND available=1;\n\
    UPDATE ivm_install_deferred SET capturing=0 WHERE source={source};\n\
    UPDATE ivm_install_jobs SET queued_rows=queued_rows+1,queued_bytes=queued_bytes+(SELECT bytes FROM main.ivm_install_journal WHERE source={source} AND revision=(SELECT revision FROM main.ivm_install_sources WHERE name={source})) WHERE source={source} AND phase IN ('scan','catchup') AND EXISTS(SELECT 1 FROM main.ivm_install_sources WHERE {identity} AND available=1);\n\
    UPDATE ivm_install_sources SET journal_rows=journal_rows+1,journal_bytes=journal_bytes+(SELECT bytes FROM main.ivm_install_journal WHERE source={source} AND revision=ivm_install_sources.revision) WHERE {identity} AND available=1;\n\
    UPDATE ivm_install_roots SET ready=0,status_revision=status_revision+1,error='{reason}' WHERE source={source} AND (ready<>0 OR error IS NOT '{reason}') AND EXISTS(SELECT 1 FROM main.ivm_install_sources WHERE name={source} AND available=0);\n\
    UPDATE ivm_install_jobs SET phase='stopped',error='{reason}' WHERE source={source} AND phase IN ('scan','catchup') AND EXISTS(SELECT 1 FROM main.ivm_install_sources WHERE name={source} AND available=0);\n",
        limits.0, limits.2, limits.1
    )
}
