//! Stage 1, test-only canonical POINT qualification. No public detail adapter, current
//! reminder-winner proof, Source registration, writer use, DDL or product size policy.
//! Every error returns no point. A successful core says nothing about another subject's
//! reminder version or the delivery/harness/runtime renderer closure.

use super::*;
use std::fmt;
use std::io::{self, Write};

const OWN_ROWS: usize = 64;
const AUX_ROWS: usize = 256;
const AUX_PER_KEY: usize = 64;
const KEY_BYTES: usize = 1024;
const REMINDER_BYTES: usize = 512;
const METADATA_BYTES: usize = 64 * 1024;
const SOURCE_BYTES: usize = 256 * 1024;
const CORE_BYTES: usize = 192 * 1024;
const CLAIM_FIELDS: [&str; 9] = [
    "id",
    "batch_id",
    "subject",
    "kind",
    "origin",
    "actor",
    "body",
    "predecessors",
    "accepted_at_unix_ms",
];
const DESIRED_FIELDS: [&str; 7] = [
    "kind",
    "revision",
    "claim_id",
    "body",
    "member",
    "owner_run",
    "owner_generation",
];

#[derive(Debug)]
enum Cause {
    TooWide {
        component: &'static str,
        limit: usize,
    },
    InvalidSource(&'static str),
    Unsupported(&'static str),
    Sql(rusqlite::Error),
    Snapshot(anyhow::Error),
    Canonical(anyhow::Error),
    Budget(crate::model::St3Error),
}

impl fmt::Display for Cause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooWide { component, limit } => {
                write!(f, "{component} exceeds qualification cap {limit}")
            }
            Self::InvalidSource(component) => {
                write!(f, "invalid qualification source: {component}")
            }
            Self::Unsupported(component) => {
                write!(f, "unsupported qualification dependency: {component}")
            }
            Self::Sql(error) => write!(f, "qualification SQL: {error}"),
            Self::Snapshot(error) => write!(f, "qualification snapshot: {error:#}"),
            Self::Canonical(error) => write!(f, "qualification canonical fold: {error:#}"),
            Self::Budget(error) => write!(f, "qualification budget: {error}"),
        }
    }
}

impl std::error::Error for Cause {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Sql(error) => Some(error),
            Self::Snapshot(error) | Self::Canonical(error) => Some(error.as_ref()),
            Self::Budget(error) => Some(error),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for Cause {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sql(error)
    }
}

type Qualified<T> = std::result::Result<T, Cause>;

fn check() -> Qualified<()> {
    smallclaims::read_budget::check().map_err(Cause::Budget)
}

fn wide(component: &'static str, limit: usize) -> Cause {
    Cause::TooWide { component, limit }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Accounting {
    own_rows: usize,
    auxiliary_rows: usize,
    metadata_bytes: usize,
    // Conservative admitted copies, including repeated calls to the canonical readers.
    // Not a SQLite page/overflow/VM or process allocation measurement.
    source_bytes: usize,
    core_bytes: usize,
}

fn charge(value: &mut usize, bytes: usize, limit: usize, component: &'static str) -> Qualified<()> {
    *value = value
        .checked_add(bytes)
        .filter(|total| *total <= limit)
        .ok_or_else(|| wide(component, limit))?;
    Ok(())
}

impl Accounting {
    fn source(&mut self, bytes: usize) -> Qualified<()> {
        charge(&mut self.source_bytes, bytes, SOURCE_BYTES, "source copies")
    }
    fn metadata(&mut self, bytes: usize) -> Qualified<()> {
        charge(&mut self.metadata_bytes, bytes, METADATA_BYTES, "metadata")
    }
    fn auxiliary(&mut self) -> Qualified<()> {
        charge(&mut self.auxiliary_rows, 1, AUX_ROWS, "auxiliary headers")?;
        self.metadata(32)
    }
}

#[derive(Debug, Serialize)]
struct ClaimEdge {
    id: String,
    accepted_at_unix_ms: u128,
}

#[derive(Debug, Serialize)]
struct PointCore {
    message: MessageView,
    first: ClaimEdge,
    last: ClaimEdge,
    session_id: Value,
}

#[derive(Debug, PartialEq, Eq)]
struct Domains {
    global_exists: bool,
    // Evidence for the canonical recipient only. Other persons still fail equality.
    person_candidate: bool,
    any_closed: bool,
    open: bool,
}

#[derive(Debug)]
struct Point {
    physical_cut: u64,
    core: PointCore,
    domains: Domains,
    reminder: Option<(String, u64)>,
    accounting: Accounting,
}

impl Point {
    fn require_public_detail(&self) -> Qualified<()> {
        if self.reminder.is_some() {
            return Err(Cause::Unsupported(
                "global/person reminder winner and superseded classification",
            ));
        }
        Err(Cause::Unsupported(
            "delivery/harness/runtime renderer closure",
        ))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    AfterOwnHeaders,
    BeforeCanonical,
    AfterCanonical,
    BeforeReturn,
}

fn capture_point(store: &Store, subject: &str) -> Qualified<Option<Point>> {
    capture_with(store, subject, |_| {})
}

fn capture_with(
    store: &Store,
    subject: &str,
    mut hook: impl FnMut(Stage),
) -> Qualified<Option<Point>> {
    check()?;
    if subject.is_empty() || subject.len() > KEY_BYTES {
        return Err(wide("subject", KEY_BYTES));
    }
    if subject.starts_with("arrangement/") {
        return Err(Cause::Unsupported(
            "arrangement actual-state dependency graph",
        ));
    }
    // Preserve typed point failures inside the one snapshot, rather than flattening them
    // into a generic snapshot error. Reentry correctly joins the actual outer physical cut.
    store
        .read_snapshot(|cut| Ok(capture_pinned(store, subject, cut, &mut hook)))
        .map_err(Cause::Snapshot)?
}

struct Header {
    index: u64,
    widths: Vec<usize>,
}

fn metadata_sql(fields: &[&str]) -> String {
    fields
        .iter()
        .map(|field| format!("typeof({field}),octet_length({field})"))
        .collect::<Vec<_>>()
        .join(",")
}

fn own_headers_sql() -> String {
    format!(
        "SELECT store_index,{} FROM claims INDEXED BY claims_subject_index
         WHERE subject=?1 AND store_index<=?2 ORDER BY store_index LIMIT 65",
        metadata_sql(&CLAIM_FIELDS),
    )
}

fn admit_fields(
    row: &rusqlite::Row<'_>,
    offset: usize,
    fields: &[&str],
    desired: bool,
    accounting: &mut Accounting,
) -> Qualified<Vec<usize>> {
    let mut widths = Vec::with_capacity(fields.len());
    for (ordinal, field) in fields.iter().enumerate() {
        check()?;
        let kind: String = row.get(offset + ordinal * 2)?;
        let bytes: Option<i64> = row.get(offset + ordinal * 2 + 1)?;
        let nullable = if desired { ordinal >= 4 } else { ordinal == 5 };
        if kind == "null" && nullable && bytes.is_none() {
            widths.push(0);
            continue;
        }
        if kind != "text" {
            return Err(Cause::InvalidSource("text column type"));
        }
        let bytes = bytes
            .and_then(|n| usize::try_from(n).ok())
            .ok_or(Cause::InvalidSource("text byte length"))?;
        let body = *field == "body" || *field == "predecessors" || *field == "member";
        if !body {
            if bytes > KEY_BYTES {
                return Err(wide("source identity", KEY_BYTES));
            }
            accounting.metadata(bytes)?;
        }
        widths.push(bytes);
    }
    Ok(widths)
}

fn capture_pinned(
    store: &Store,
    subject: &str,
    cut: u64,
    hook: &mut impl FnMut(Stage),
) -> Qualified<Option<Point>> {
    check()?;
    if !store.readers.has_pinned_reader() {
        return Err(Cause::InvalidSource("missing registered reader pin"));
    }
    let db = store.readers.get();
    if db.is_autocommit() || current_index(&db).map_err(Cause::Canonical)? != cut {
        return Err(Cause::InvalidSource("physical snapshot mismatch"));
    }
    let encoding: String = db.query_row("PRAGMA encoding", [], |row| row.get(0))?;
    if encoding != "UTF-8" {
        return Err(Cause::Unsupported("database encoding"));
    }
    let mut accounting = Accounting::default();
    accounting.source(subject.len())?;
    let query = own_headers_sql();
    let mut statement = db.prepare_cached(&query)?;
    let mut rows = statement.query(params![subject, cut])?;
    let mut headers = Vec::new();
    while let Some(row) = rows.next()? {
        check()?;
        if headers.len() == OWN_ROWS {
            return Err(wide("own claims", OWN_ROWS));
        }
        let index: u64 = row.get(0)?;
        accounting.metadata(CLAIM_FIELDS.len() * 32)?;
        let widths = admit_fields(row, 1, &CLAIM_FIELDS, false, &mut accounting)?;
        // IDs/batch IDs in preflight, kind/body in latest_actual, all fields in claims_for.
        let bytes = widths
            .iter()
            .try_fold(0usize, |n, b| n.checked_add(*b))
            .ok_or_else(|| wide("source copies", SOURCE_BYTES))?;
        accounting.source(bytes)?;
        // Also admit a conservative JSON-source copy in the sent-recipient SQL.
        for ordinal in [0, 1, 3, 6, 6] {
            accounting.source(widths[ordinal])?;
        }
        headers.push(Header { index, widths });
    }
    drop(rows);
    drop(statement);
    accounting.own_rows = headers.len();
    hook(Stage::AfterOwnHeaders);
    check()?;
    if headers.is_empty() {
        return Ok(None);
    }

    let mut batches = BTreeSet::new();
    let mut ids = Vec::with_capacity(headers.len());
    for header in &headers {
        check()?;
        let (id, batch): (String, String) = db.query_row(
            "SELECT id,batch_id FROM claims WHERE store_index=?1",
            [header.index],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if id.len() != header.widths[0] || batch.len() != header.widths[1] {
            return Err(Cause::InvalidSource("admitted identity changed"));
        }
        ids.push(id);
        batches.insert(batch);
    }
    for batch in batches {
        admit_batch(&db, &batch, &mut accounting)?;
    }
    for id in &ids {
        admit_replica_and_repair(&db, id, &mut accounting)?;
    }
    admit_desired(&db, subject, &mut accounting)?;
    let global = admit_global(&db, subject, headers[0].index, &mut accounting)?;
    hook(Stage::BeforeCanonical);
    check()?;

    let created = headers[0].index;
    let message = message_view_tx(&db, subject, created).map_err(Cause::Canonical)?;
    check()?;
    if message.to.len() > KEY_BYTES {
        return Err(wide("canonical recipient", KEY_BYTES));
    }
    let reminder = message_reminder_rank(&message.tags)
        .map(|(key, version)| {
            if key.len() > REMINDER_BYTES {
                return Err(wide("reminder", REMINDER_BYTES));
            }
            Ok((key.to_owned(), version))
        })
        .transpose()?;
    let (person_candidate, any_closed) = domains(&db, subject, &message.to)?;
    if global.is_some_and(|closed| closed != any_closed) {
        return Err(Cause::InvalidSource("global message closed state"));
    }
    let global_exists = global.is_some();
    let open = !any_closed && message.status != "closed";
    // Literal existing reader; the pin and preflight cover every joined source field.
    let history = store.claims_for(subject, None).map_err(Cause::Canonical)?;
    check()?;
    let first = history
        .first()
        .ok_or(Cause::InvalidSource("canonical history missing"))?;
    let last = history
        .last()
        .ok_or(Cause::InvalidSource("canonical history missing"))?;
    let session_id = first
        .body
        .get("fields")
        .unwrap_or(&first.body)
        .get("session_id")
        .cloned()
        .unwrap_or(Value::Null);
    let core = PointCore {
        message,
        first: ClaimEdge {
            id: first.id.clone(),
            accepted_at_unix_ms: first.accepted_at_unix_ms,
        },
        last: ClaimEdge {
            id: last.id.clone(),
            accepted_at_unix_ms: last.accepted_at_unix_ms,
        },
        session_id,
    };
    hook(Stage::AfterCanonical);
    check()?;
    accounting.core_bytes = admitted_core_bytes(&core)?;
    hook(Stage::BeforeReturn);
    check()?;
    Ok(Some(Point {
        physical_cut: cut,
        core,
        domains: Domains {
            global_exists,
            person_candidate,
            any_closed,
            open,
        },
        reminder,
        accounting,
    }))
}

fn admit_batch(db: &Connection, batch: &str, accounting: &mut Accounting) -> Qualified<()> {
    check()?;
    let row = db.query_row(
        "SELECT typeof(id),octet_length(id),typeof(origin),octet_length(origin),
         typeof(replica_sequence),CASE WHEN typeof(replica_sequence)='integer' THEN replica_sequence END
         FROM batches WHERE id=?1", [batch], |row| Ok((
            row.get::<_, String>(0)?, row.get::<_, i64>(1)?, row.get::<_, String>(2)?,
            row.get::<_, i64>(3)?, row.get::<_, String>(4)?, row.get::<_, Option<i64>>(5)?,
         )),
    ).optional()?.ok_or(Cause::InvalidSource("missing canonical batch"))?;
    accounting.auxiliary()?;
    if row.0 != "text" || row.2 != "text" || row.4 != "integer" || row.5.is_none_or(|n| n < 0) {
        return Err(Cause::InvalidSource("canonical batch type/range"));
    }
    for bytes in [row.1, row.3] {
        let bytes = usize::try_from(bytes).map_err(|_| Cause::InvalidSource("batch length"))?;
        if bytes > KEY_BYTES {
            return Err(wide("batch identity", KEY_BYTES));
        }
        accounting.metadata(bytes)?;
    }
    // canonical::position_sql's legacy COUNT is not bounded by a subject LIMIT.
    // Admit the entire referenced batch's cardinality before any canonical ORDER BY.
    let mut statement = db.prepare_cached(
        "SELECT store_index FROM claims INDEXED BY claims_batch_index
         WHERE batch_id=?1 ORDER BY store_index LIMIT ?2",
    )?;
    let mut rows = statement.query(params![batch, (AUX_PER_KEY + 1) as u64])?;
    let mut count = 0;
    while let Some(row) = rows.next()? {
        check()?;
        if count == AUX_PER_KEY {
            return Err(wide("legacy batch members", AUX_PER_KEY));
        }
        let _: u64 = row.get(0)?;
        accounting.auxiliary()?;
        count += 1;
    }
    Ok(())
}

fn admit_replica_and_repair(
    db: &Connection,
    id: &str,
    accounting: &mut Accounting,
) -> Qualified<()> {
    check()?;
    let mut statement = db.prepare_cached(
        "SELECT typeof(position),CASE WHEN typeof(position)='integer' THEN position END
         FROM replica_records INDEXED BY replica_records_claim
         WHERE claim_id=?1 ORDER BY position LIMIT ?2",
    )?;
    let mut rows = statement.query(params![id, (AUX_PER_KEY + 1) as u64])?;
    let mut count = 0;
    while let Some(row) = rows.next()? {
        check()?;
        if count == AUX_PER_KEY {
            return Err(wide("replica positions", AUX_PER_KEY));
        }
        let kind: String = row.get(0)?;
        let value: Option<i64> = row.get(1)?;
        if kind != "integer" || value.is_none_or(|n| n < 0) {
            return Err(Cause::InvalidSource("replica position type/range"));
        }
        accounting.auxiliary()?;
        count += 1;
    }
    drop(rows);
    drop(statement);
    let repaired = db
        .query_row(
            "SELECT typeof(id),octet_length(id) FROM projection_digest_repaired_claims WHERE id=?1",
            [id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()?;
    if let Some((kind, bytes)) = repaired {
        accounting.auxiliary()?;
        if kind != "text" || usize::try_from(bytes).ok() != Some(id.len()) {
            return Err(Cause::InvalidSource("repaired claim identity"));
        }
    }
    Ok(())
}

fn admit_desired(db: &Connection, subject: &str, accounting: &mut Accounting) -> Qualified<()> {
    check()?;
    let sql = format!(
        "SELECT {} FROM desired WHERE subject=?1",
        metadata_sql(&DESIRED_FIELDS)
    );
    let mut statement = db.prepare_cached(&sql)?;
    let mut rows = statement.query([subject])?;
    if let Some(row) = rows.next()? {
        accounting.auxiliary()?;
        accounting.metadata(DESIRED_FIELDS.len() * 32)?;
        let widths = admit_fields(row, 0, &DESIRED_FIELDS, true, accounting)?;
        for width in &widths {
            accounting.source(*width)?;
        }
        // current_desired_row and the desired-child candidate SQL both consume body.
        accounting.source(widths[3])?;
    }
    Ok(())
}

fn admit_global(
    db: &Connection,
    subject: &str,
    created: u64,
    accounting: &mut Accounting,
) -> Qualified<Option<bool>> {
    check()?;
    let row = db.query_row(
        "SELECT typeof(created_index),CASE WHEN typeof(created_index)='integer' THEN created_index END,
         typeof(closed),CASE WHEN typeof(closed)='integer' THEN closed END
         FROM message_index WHERE subject=?1", [subject],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<i64>>(1)?,
            row.get::<_, String>(2)?, row.get::<_, Option<i64>>(3)?)),
    ).optional()?;
    let Some((index_type, index, closed_type, closed)) = row else {
        return Ok(None);
    };
    accounting.auxiliary()?;
    if index_type != "integer"
        || index.and_then(|n| u64::try_from(n).ok()) != Some(created)
        || closed_type != "integer"
        || !matches!(closed, Some(0 | 1))
    {
        return Err(Cause::InvalidSource(
            "global message index type/range/position",
        ));
    }
    Ok(Some(closed == Some(1)))
}

fn domains(db: &Connection, subject: &str, recipient: &str) -> Qualified<(bool, bool)> {
    check()?;
    // Same literal predicates used by messages_page_inner, now constrained to this
    // already-admitted subject before JSON. No whole-mailbox CTE or desired-kind scan.
    let sql = concat!(
        "SELECT EXISTS(SELECT 1 FROM claims INDEXED BY claims_subject_kind_index
          WHERE subject=?3 AND kind='message.sent' AND ",
        message_sent_recipient_match!(),
        "),
          EXISTS(SELECT 1 FROM desired CROSS JOIN json_each(desired.body,'$.children') child
          WHERE desired.subject=?3 AND desired.kind='message' AND ",
        message_desired_recipient_match!(),
        "),
          EXISTS(SELECT 1 FROM claims INDEXED BY claims_subject_kind_index
          WHERE subject=?3 AND kind='message.closed')",
    );
    let normalized = normalize_message_party(recipient);
    let bare = message_bare_recipient(&normalized);
    let (sent, desired, closed): (bool, bool, bool) =
        db.query_row(sql, params![normalized, bare, subject], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
    check()?;
    Ok((sent || desired, closed))
}

struct CoreSize {
    bytes: usize,
    exceeded: bool,
}

impl Write for CoreSize {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        smallclaims::read_budget::check().map_err(io::Error::other)?;
        if self
            .bytes
            .checked_add(bytes.len())
            .is_none_or(|n| n > CORE_BYTES)
        {
            self.exceeded = true;
            return Err(io::Error::other("point core qualification cap"));
        }
        self.bytes += bytes.len();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn admitted_core_bytes(core: &PointCore) -> Qualified<usize> {
    let mut size = CoreSize {
        bytes: 0,
        exceeded: false,
    };
    if let Err(error) = serde_json::to_writer(&mut size, core) {
        check()?;
        if size.exceeded {
            return Err(wide("point core output", CORE_BYTES));
        }
        return Err(Cause::Canonical(error.into()));
    }
    check()?;
    Ok(size.bytes)
}

#[cfg(test)]
mod tests;
