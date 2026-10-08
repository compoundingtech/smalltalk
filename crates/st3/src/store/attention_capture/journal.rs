//! Bind one Installer journal prefix to owned native OLD/NEW images. No source
//! qualification, bootstrap, family reduction or producer registration occurs here.
//! Locator/layout work is reserved before the existing bounded image extractor;
//! body and primary-key decoding uses owned data after snapshot release. REAL
//! cells refuse until the native image ABI can certify exact floating-point bits.
use super::*;
use anyhow::Context;
use rusqlite::types::Value as Cell;
use serde_json::Value;
use smallclaims::ivm::install::prepared::PreparedPage;
use std::collections::BTreeMap;

const ROWS: usize = 64;
const BYTES: usize = 256 * 1024;

pub(crate) struct Budget {
    pub rows: usize,
    pub bytes: usize,
}
struct Request {
    key: String,
    table: Table,
    revision: u64,
    old: bool,
    new: bool,
}
pub(crate) struct JournalBatch {
    position: SourcePosition,
    snapshot_position: SourcePosition,
    requests: Vec<Request>,
    images: Vec<RetainedImage>,
    work_rows: usize,
    work_bytes: usize,
}
pub(crate) struct ImageChange {
    pub key: String,
    pub table: &'static str,
    pub revision: u64,
    pub old: Option<BTreeMap<String, Cell>>,
    pub new: Option<BTreeMap<String, Cell>>,
}
pub(crate) struct BoundJournal {
    /// Applied prefix; never relabel immutable images with the snapshot frontier.
    pub position: SourcePosition,
    pub snapshot_position: SourcePosition,
    pub changes: Vec<ImageChange>,
    /// Includes layout/locator checks, point-query rows and owned input decoding.
    /// Actual SQLite VM cost, native capture completeness and reducer/output work
    /// are separately owed by the source. These counters are not time bounds.
    pub work_rows: usize,
    pub work_bytes: usize,
}
struct Work {
    rows: usize,
    bytes: usize,
    budget: Budget,
}
impl Work {
    fn add(&mut self, rows: usize, bytes: usize) -> Result<()> {
        self.rows = self
            .rows
            .checked_add(rows)
            .context("attention journal rows")?;
        self.bytes = self
            .bytes
            .checked_add(bytes)
            .context("attention journal bytes")?;
        ensure!(
            self.rows <= self.budget.rows,
            "attention journal row budget"
        );
        ensure!(
            self.bytes <= self.budget.bytes,
            "attention journal byte budget"
        );
        Ok(())
    }
}

impl NativeCapture {
    /// Use the SAME established main READ snapshot that captured catch-up/live
    /// PreparedPage. Scan/bootstrap rows are not native journal locators.
    pub(crate) fn journal_batch(
        &self,
        c: &Connection,
        page: &PreparedPage,
        budget: Budget,
    ) -> Result<JournalBatch> {
        self.capture_journal(
            c,
            page.position(),
            page.snapshot_position(),
            page.rows(),
            budget,
        )
    }

    fn capture_journal(
        &self,
        c: &Connection,
        position: &SourcePosition,
        snapshot: &SourcePosition,
        rows: &[Mutation],
        budget: Budget,
    ) -> Result<JournalBatch> {
        ensure!(
            !c.is_autocommit()
                && unsafe { rusqlite::ffi::sqlite3_txn_state(c.handle(), c"main".as_ptr()) }
                    == rusqlite::ffi::SQLITE_TXN_READ,
            "attention journal requires established main READ cut"
        );
        ensure!(
            (4..=ROWS).contains(&budget.rows) && (1..=BYTES).contains(&budget.bytes),
            "attention journal budget"
        );
        let mut work = Work {
            rows: 0,
            bytes: 0,
            budget,
        };
        for cut in [position, snapshot] {
            work.add(1, 64 + self.source.len() + self.fingerprint.len())?;
            ensure!(
                cut.source == self.source
                    && cut.fingerprint == self.fingerprint
                    && cut.epoch == self.epoch
                    && cut.revision <= i64::MAX as u64,
                "attention journal source identity"
            );
        }
        ensure!(
            position.revision <= snapshot.revision,
            "attention journal future prefix"
        );
        ensure!(rows.len() <= ROWS, "attention journal reference count");
        ensure!(
            !rows.is_empty() || position.revision == snapshot.revision,
            "attention journal empty incomplete prefix"
        );
        let first = position
            .revision
            .checked_sub(rows.len() as u64)
            .context("attention journal prefix length")?;
        let mut requests = Vec::with_capacity(rows.len());
        let mut sides = 0usize;
        for (offset, row) in rows.iter().enumerate() {
            ensure!(
                !row.key.is_empty() && row.key.len() <= 4096,
                "attention journal key bound"
            );
            // The image extractor parses the key again and checks its existing
            // schema registry. Reserve that registry walk before any image SQL.
            work.add(4 + self.schema.len(), row.key.len() * 2)?;
            let revision = first
                .checked_add(offset as u64 + 1)
                .context("attention journal revision overflow")?;
            let mut selected = None;
            for (side, locator) in [(0, row.old.as_ref()), (1, row.new.as_ref())] {
                let Some(locator) = locator else { continue };
                work.add(4, 64)?;
                let fields = locator
                    .as_object()
                    .context("attention journal locator object")?;
                ensure!(fields.len() == 3, "attention journal locator fields");
                let name = fields
                    .get("table")
                    .and_then(Value::as_str)
                    .context("attention journal locator table")?;
                ensure!(name.len() <= 128, "attention journal locator table bound");
                ensure!(
                    fields.get("revision").and_then(Value::as_u64) == Some(revision)
                        && fields.get("side").and_then(Value::as_u64) == Some(side),
                    "attention journal locator prefix/side"
                );
                let mut table = None;
                for candidate in &self.tables {
                    work.add(1, candidate.name.len())?;
                    if candidate.name == name {
                        table = Some(*candidate);
                        break;
                    }
                }
                let table = table.context("attention journal table not registered")?;
                ensure!(
                    selected.is_none_or(|previous: Table| previous.name == table.name),
                    "attention journal replacement table changed"
                );
                selected = Some(table);
                // Existing extractor performs a metadata seek and a body seek per side.
                // Reserve complete layout validation, typed PK comparison, and those rows.
                work.add(2 + table.columns.len() + table.key.len(), 64 + name.len())?;
                sides += 1;
            }
            let table = selected.context("attention journal empty mutation")?;
            requests.push(Request {
                key: row.key.clone(),
                table,
                revision,
                old: row.old.is_some(),
                new: row.new.is_some(),
            });
        }
        // One exact source/capture point join plus the extractor's TEMP-metadata refusal.
        work.add(2, 64)?;
        let revision: u64 = c.query_row(
            "SELECT s.revision FROM main.ivm_install_sources s
             JOIN main.local_attention_native_capture n ON n.source=s.name AND n.epoch=s.epoch
             WHERE s.name=?1 AND s.fingerprint=?2 AND s.epoch=?3 AND s.available=1
               AND n.managed=0 AND n.reclaiming=0",
            params![self.source, self.fingerprint, self.epoch],
            |r| r.get(0),
        )?;
        ensure!(
            revision == snapshot.revision,
            "attention journal snapshot frontier changed"
        );
        let images = if sides == 0 {
            ensure!(
                installer_metadata_unshadowed(c)?,
                "attention journal metadata shadow"
            );
            vec![]
        } else {
            // Charge three encoded input copies for capture/decoding/binding. This
            // conservative accounting is not a heap-allocation or VM-work proof.
            // Oversized pages refuse rather than silently truncating a side.
            let image_bytes = (work.budget.bytes - work.bytes) / 3;
            ensure!(image_bytes > 0, "attention journal image byte budget");
            self.capture_images(c, snapshot, rows, sides, image_bytes)?
        };
        ensure!(images.len() == sides, "attention journal image count");
        let mut index = 0;
        for request in &requests {
            for present in [request.old, request.new] {
                if !present {
                    continue;
                }
                let image = &images[index];
                let bytes = image
                    .body
                    .len()
                    .checked_add(image.table.len())
                    .and_then(|n| n.checked_add(request.key.len() + 32))
                    .and_then(|n| n.checked_mul(3))
                    .context("attention journal image bytes")?;
                work.add(0, bytes)?;
                index += 1;
            }
        }
        Ok(JournalBatch {
            position: position.clone(),
            snapshot_position: snapshot.clone(),
            requests,
            images,
            work_rows: work.rows,
            work_bytes: work.bytes,
        })
    }
}

impl JournalBatch {
    /// Call after releasing capture's snapshot. No SQLite/Store/clock handle is retained.
    pub(crate) fn decode(self) -> Result<BoundJournal> {
        let mut images = self.images.into_iter();
        let mut changes = Vec::with_capacity(self.requests.len());
        for request in self.requests {
            let key: Value = serde_json::from_str(&request.key)?;
            let key = key.as_array().context("attention journal key layout")?;
            ensure!(
                key.len() == 2 && key[0].as_str() == Some(request.table.name),
                "attention journal key table/layout"
            );
            let values = key[1].as_array().context("attention journal key values")?;
            ensure!(
                values.len() == request.table.key.len(),
                "attention journal full primary key"
            );
            let mut take = |side: usize, present: bool| -> Result<Option<BTreeMap<String, Cell>>> {
                if !present {
                    return Ok(None);
                }
                let image = images.next().context("attention journal missing image")?;
                ensure!(
                    image.revision == request.revision
                        && image.side == side
                        && image.table == request.table.name,
                    "attention journal image order/binding"
                );
                let cells = image.decode()?;
                ensure!(
                    cells.values().all(|cell| !matches!(cell, Cell::Real(_))),
                    "attention journal REAL image fidelity not qualified"
                );
                ensure!(
                    cells.len() == request.table.columns.len()
                        && request
                            .table
                            .columns
                            .iter()
                            .all(|column| cells.contains_key(*column)),
                    "attention journal complete image layout"
                );
                for (column, value) in request.table.key.iter().zip(values) {
                    ensure!(
                        cells
                            .get(*column)
                            .is_some_and(|cell| key_matches(value, cell)),
                        "attention journal primary key/image mismatch"
                    );
                }
                Ok(Some(cells))
            };
            let old = take(0, request.old)?;
            let new = take(1, request.new)?;
            changes.push(ImageChange {
                key: request.key,
                table: request.table.name,
                revision: request.revision,
                old,
                new,
            });
        }
        ensure!(images.next().is_none(), "attention journal extra image");
        Ok(BoundJournal {
            position: self.position,
            snapshot_position: self.snapshot_position,
            changes,
            work_rows: self.work_rows,
            work_bytes: self.work_bytes,
        })
    }
}
fn key_matches(key: &Value, cell: &Cell) -> bool {
    match (key, cell) {
        (Value::String(key), Cell::Text(cell)) => key == cell,
        (Value::Number(key), Cell::Integer(cell)) => key.as_i64() == Some(*cell),
        (Value::Object(key), Cell::Blob(cell)) if key.len() == 1 => {
            let Some(key) = key.get("$blob").and_then(Value::as_str) else {
                return false;
            };
            if key.len() != cell.len() * 2 {
                return false;
            }
            const HEX: &[u8; 16] = b"0123456789ABCDEF";
            key.as_bytes()
                .chunks_exact(2)
                .zip(cell)
                .all(|(pair, byte)| {
                    pair[0] == HEX[usize::from(byte >> 4)] && pair[1] == HEX[usize::from(byte & 15)]
                })
        }
        // Numeric JSON cannot certify exact REAL keys; do not erase storage classes.
        _ => false,
    }
}

#[cfg(test)]
mod tests;
