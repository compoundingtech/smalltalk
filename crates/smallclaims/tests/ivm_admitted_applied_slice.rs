//! Source-only next slice: real file/WAL Store, existing Installer journal and prepared pages.
//! All helpers are private to this fixture. No production Runtime/provider/worker is installed.
//! Rows/bytes/fanout are checked here; metadata VM cost and failed-cleanup quarantine are NOT
//! qualified by this fixture. See src/ivm/ADMITTED_APPLIED_SLICE.md for the remaining obligations.

use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, Transaction, params, types::Value as Sql};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use smallclaims::{
    ClaimInput, ClaimRecord, Store,
    ivm::install::{
        Installer, Limits, Mutation, Namespace, Operator, Outcome, ScanPage,
        capture::Change,
        prepared::{CaptureLimits, PreparedPage, PublicationLimits},
    },
    store::runtime::{Plain, Runtime},
};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

const SOURCE: &str = "fixture.admitted-claims";
const VIEW: &str = "fixture.claim-ledger";
const IMAGE_BYTES: usize = 8192;
const PAGE_ROWS: usize = 16;
const PAGE_BYTES: usize = PAGE_ROWS * IMAGE_BYTES;
const PENDING_ROWS: u64 = 256;
const PENDING_BYTES: u64 = PENDING_ROWS * IMAGE_BYTES as u64;
const REFERENCE_BYTES: usize = 512;
const JOURNAL_BYTES: u64 = PENDING_ROWS * REFERENCE_BYTES as u64;
const TOTAL_ROWS: u64 = 4096;

struct Ledger;
impl Operator for Ledger {
    fn name(&self) -> &'static str {
        VIEW
    }
    fn source(&self) -> &'static str {
        SOURCE
    }
    fn fingerprint(&self) -> &'static str {
        "fixture.claim-ledger.v1"
    }
    fn create_schema(&self, db: &Connection) -> Result<()> {
        db.execute_batch(
            "CREATE TABLE main.slice_images(
                revision INTEGER PRIMARY KEY, store_index INTEGER NOT NULL UNIQUE,
                claim_id TEXT NOT NULL UNIQUE,
                subject TEXT NOT NULL, kind TEXT NOT NULL, body BLOB NOT NULL,
                bytes INTEGER NOT NULL);
             CREATE TABLE main.slice_output(
                namespace TEXT NOT NULL, claim_id TEXT NOT NULL, subject TEXT NOT NULL,
                kind TEXT NOT NULL, digest TEXT NOT NULL, PRIMARY KEY(namespace,claim_id));
             CREATE TABLE main.slice_coverage(
                namespace TEXT PRIMARY KEY NOT NULL, through INTEGER NOT NULL);
             CREATE TABLE main.slice_limits(
                id INTEGER PRIMARY KEY CHECK(id=1), retained_rows INTEGER NOT NULL,
                retained_bytes INTEGER NOT NULL, total_rows INTEGER NOT NULL,
                capturing INTEGER NOT NULL, draining INTEGER NOT NULL);
             INSERT INTO main.slice_limits VALUES(1,0,0,0,0,0);",
        )?;
        Ok(())
    }
    fn apply(&self, _: &Transaction<'_>, _: &Namespace, _: &[Mutation]) -> Result<bool> {
        bail!("fixture reducers must not run on the writer")
    }
    fn validate_publication(&self, _: &Transaction<'_>, _: &Namespace) -> Result<()> {
        bail!("fixture validation callbacks must not run on the writer")
    }
    fn reclaim(&self, tx: &Transaction<'_>, namespace: &Namespace, rows: usize) -> Result<bool> {
        ensure!(
            (1..=PAGE_ROWS).contains(&rows),
            "invalid fixture reclaim page"
        );
        let removed = tx.execute(
            "DELETE FROM main.slice_output WHERE namespace=?1 AND claim_id IN
             (SELECT claim_id FROM main.slice_output WHERE namespace=?1 ORDER BY claim_id LIMIT ?2)",
            params![namespace.as_str(), rows],
        )?;
        if removed < rows {
            tx.execute(
                "DELETE FROM main.slice_coverage WHERE namespace=?1",
                [namespace.as_str()],
            )?;
        }
        Ok(!tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM main.slice_output WHERE namespace=?1)",
            [namespace.as_str()],
            |r| r.get::<_, bool>(0),
        )? && !tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM main.slice_coverage WHERE namespace=?1)",
            [namespace.as_str()],
            |r| r.get::<_, bool>(0),
        )?)
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct Manifest {
    device: u64,
    inode: u64,
    schema: i64,
    fingerprint: String,
    job: String,
}

struct Slice {
    store: Store,
    installer: Installer,
    path: PathBuf,
    manifest: Manifest,
    available: AtomicBool,
}

struct Image {
    store_index: u64,
    id: String,
    subject: String,
    kind: String,
    body: Vec<u8>,
}
struct OwnedPage {
    prepared: PreparedPage,
    images: Vec<Image>,
    bytes: usize,
}

fn installer() -> Result<Installer> {
    Installer::new(vec![Box::new(Ledger)])
}
fn schema(db: &Connection) -> Result<i64> {
    Ok(db.query_row("PRAGMA main.schema_version", [], |r| r.get(0))?)
}
fn manifest_path(path: &Path) -> PathBuf {
    path.with_extension("slice-lifetime.json")
}
fn limits() -> PublicationLimits {
    // Accounting includes input rows, output writes, coverage write and evidence check.
    PublicationLimits {
        rows: PAGE_ROWS * 2 + 2,
        bytes: PAGE_BYTES + 32768,
        tables: 2,
    }
}

impl Slice {
    fn create(path: &Path) -> Result<Self> {
        let store = Store::open(path, "slice-fixture", Arc::new(Plain))?;
        let installer = installer()?;
        let fingerprint = format!("fixture.claim-images.v1:{}", uuid::Uuid::now_v7());
        let (job, cookie) = {
            let mut writer = store.connection.write_background();
            ensure!(
                writer.is_autocommit(),
                "fixture setup inherited a transaction"
            );
            ensure!(
                !writer.query_row("SELECT EXISTS(SELECT 1 FROM main.claims)", [], |r| r
                    .get::<_, bool>(0))?,
                "fixture setup requires a genuinely empty source"
            );
            let tx = writer.transaction()?;
            installer.create_schema(&tx)?;
            installer.register_source(&tx, SOURCE, &fingerprint, 1)?;
            let position = installer.position(&tx, SOURCE)?;
            installer.enable_deferred(
                &tx,
                &position,
                CaptureLimits {
                    rows: PENDING_ROWS,
                    bytes: JOURNAL_BYTES,
                    reference_bytes: REFERENCE_BYTES,
                },
            )?;
            // claims.id is UNIQUE; its actual full physical PRIMARY KEY is store_index.
            let plan = installer.capture_plan(&tx, &position, "claims", &["store_index"])?;
            // Explicit fixture setup only. Trigger DML is unqualified under SQLite's
            // same-database trigger rule; native targets and external reads bind main.
            // No JSON body parsing: byte guards precede the bounded image copy.
            let width = "typeof(NEW.store_index)='integer' AND NEW.store_index>0
                AND typeof(NEW.id)='text' AND octet_length(NEW.id)<=128
                AND typeof(NEW.subject)='text' AND octet_length(NEW.subject)<=256
                AND typeof(NEW.kind)='text' AND octet_length(NEW.kind)<=128
                AND typeof(NEW.body)='text'
                AND 8+octet_length(NEW.id)+octet_length(NEW.subject)+octet_length(NEW.kind)+octet_length(NEW.body)<=8192";
            tx.execute_batch(&format!(
                "CREATE TRIGGER main.slice_admit AFTER INSERT ON main.claims BEGIN
                   UPDATE ivm_install_sources SET available=0 WHERE name='{SOURCE}' AND
                     (NOT({width}) OR EXISTS(SELECT 1 FROM slice_images WHERE claim_id=NEW.id)
                       OR EXISTS(SELECT 1 FROM slice_images WHERE store_index=NEW.store_index)
                       OR EXISTS(SELECT 1 FROM slice_output WHERE namespace=(SELECT namespace FROM ivm_install_roots WHERE view='{VIEW}') AND claim_id=NEW.id)
                       OR EXISTS(SELECT 1 FROM slice_limits WHERE id=1 AND
                       (retained_rows>=256 OR total_rows>=4096 OR
                        retained_bytes>2097152-(8+octet_length(NEW.id)+octet_length(NEW.subject)+octet_length(NEW.kind)+octet_length(NEW.body)))));
                   UPDATE slice_limits SET capturing=1 WHERE id=1;
                   {append}
                   INSERT INTO slice_images SELECT {revision},NEW.store_index,NEW.id,NEW.subject,NEW.kind,CAST(NEW.body AS BLOB),
                     8+octet_length(NEW.id)+octet_length(NEW.subject)+octet_length(NEW.kind)+octet_length(NEW.body)
                     WHERE {available};
                   UPDATE slice_limits SET capturing=0 WHERE id=1;
                 END;
                 CREATE TRIGGER main.slice_claim_update AFTER UPDATE ON main.claims BEGIN {gap} END;
                 CREATE TRIGGER main.slice_claim_delete AFTER DELETE ON main.claims BEGIN {gap} END;
                 CREATE TRIGGER main.slice_image_insert AFTER INSERT ON main.slice_images BEGIN
                   UPDATE slice_limits SET retained_rows=retained_rows+1,retained_bytes=retained_bytes+NEW.bytes,
                     total_rows=total_rows+1 WHERE id=1;
                   UPDATE ivm_install_sources SET available=0 WHERE name='{SOURCE}' AND
                     (SELECT capturing FROM slice_limits WHERE id=1)<>1;
                 END;
                 CREATE TRIGGER main.slice_image_update AFTER UPDATE ON main.slice_images BEGIN {gap} END;
                 CREATE TRIGGER main.slice_image_delete AFTER DELETE ON main.slice_images BEGIN
                   UPDATE slice_limits SET retained_rows=retained_rows-1,retained_bytes=retained_bytes-OLD.bytes WHERE id=1;
                   UPDATE ivm_install_sources SET available=0 WHERE name='{SOURCE}' AND
                     (SELECT draining FROM slice_limits WHERE id=1)<>1;
                 END;",
                append=plan.append_sql(Change::Insert), revision=plan.revision_sql(),
                available=plan.available_sql(), gap=plan.gap_sql(),
            ))?;
            let job = installer.start(
                &tx,
                VIEW,
                Limits {
                    page_rows: PAGE_ROWS,
                    page_bytes: PAGE_BYTES,
                    pending_rows: PENDING_ROWS,
                    pending_bytes: JOURNAL_BYTES,
                    total_rows: TOTAL_ROWS,
                    callback_ms: 1000,
                    lifetime_ms: 60000,
                },
                0,
            )?;
            let cookie = schema(&tx)?;
            tx.commit()?;
            (job, cookie)
        };
        let stamp = std::fs::metadata(path)?;
        let manifest = Manifest {
            device: stamp.dev(),
            inode: stamp.ino(),
            schema: cookie,
            fingerprint,
            job,
        };
        // Fixture-owned identity is external to the copied database, never discovered/rebound
        // from source metadata. This is not an all-instance production incarnation contract.
        std::fs::write(manifest_path(path), serde_json::to_vec(&manifest)?)?;
        let slice = Self {
            store,
            installer,
            path: path.into(),
            manifest,
            available: AtomicBool::new(true),
        };
        slice.bootstrap()?;
        Ok(slice)
    }

    fn open(path: &Path) -> Result<Self> {
        let mut bytes = Vec::new();
        std::fs::File::open(manifest_path(path))?
            .take(2049)
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= 2048,
            "fixture lifetime manifest exceeds bound"
        );
        let manifest: Manifest = serde_json::from_slice(&bytes)?;
        ensure!(
            manifest.fingerprint.len() <= 128 && manifest.job.len() <= 128,
            "fixture identity exceeds bound"
        );
        // Plain has no derived slice getter/startup replay. The process gate is closed before
        // opening native Store; only this fixture's getters can expose its output.
        let mut slice = Self {
            store: Store::open(path, "slice-fixture", Arc::new(Plain))?,
            installer: installer()?,
            path: path.into(),
            manifest,
            available: AtomicBool::new(false),
        };
        let compatible = slice.read(|db| slice.identity(db)).is_ok();
        slice.available = AtomicBool::new(compatible);
        Ok(slice)
    }

    fn identity(&self, db: &Connection) -> Result<()> {
        let stamp = std::fs::metadata(&self.path)?;
        ensure!(
            (stamp.dev(), stamp.ino()) == (self.manifest.device, self.manifest.inode),
            "fixture file lifetime changed"
        );
        ensure!(
            schema(db)? == self.manifest.schema,
            "fixture schema lifetime changed"
        );
        // Existing unqualified Installer helpers require an explicit no-TEMP-shadow precondition.
        ensure!(
            !db.query_row("SELECT EXISTS(SELECT 1 FROM temp.sqlite_schema)", [], |r| r
                .get::<_, bool>(0))?,
            "fixture TEMP metadata is unsupported"
        );
        let position = self.installer.position(db, SOURCE)?;
        ensure!(
            position.fingerprint == self.manifest.fingerprint && position.epoch == 1,
            "fixture source identity changed"
        );
        Ok(())
    }

    fn read<T>(&self, capture: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        smallclaims::read_budget::check()?;
        let mut guard = self.store.readers.try_get()?;
        ensure!(
            guard.pinned.is_none(),
            "fixture inherited a pinned/request reader"
        );
        let db = guard
            .connection
            .take()
            .context("fixture does not own its physical reader")?;
        let idle = unsafe { rusqlite::ffi::sqlite3_txn_state(db.handle(), c"main".as_ptr()) == 0 };
        if !db.is_autocommit() || !idle {
            self.available.store(false, Ordering::Release);
            bail!("fixture acquired an active reader"); // owned db is discarded
        }
        if let Err(error) = db.execute_batch("BEGIN DEFERRED") {
            self.available.store(false, Ordering::Release);
            return Err(error.into()); // owned db is discarded
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<T> {
            let value = capture(&db)?;
            smallclaims::read_budget::check()?;
            Ok(value)
        }));
        let state = unsafe { rusqlite::ffi::sqlite3_txn_state(db.handle(), c"main".as_ptr()) };
        let changed_cut =
            db.is_autocommit() || state == 2 || (matches!(&result, Ok(Ok(_))) && state != 1);
        // This fixture owns the physical reader. Remove cancellation for cleanup and require
        // main NONE as well as autocommit before returning it to the normal pool.
        db.progress_handler(0, None::<fn() -> bool>);
        let exit = db.execute_batch("ROLLBACK");
        let idle = unsafe { rusqlite::ffi::sqlite3_txn_state(db.handle(), c"main".as_ptr()) == 0 };
        if changed_cut || exit.is_err() || !db.is_autocommit() || !idle {
            self.available.store(false, Ordering::Release);
            drop(db); // exact uncertain reader is physically closed, never placed back in guard
            bail!("fixture read exit unsafe");
        }
        guard.connection = Some(db);
        drop(guard);
        match result {
            Ok(result) => result,
            Err(panic) => std::panic::resume_unwind(panic),
        }
    }

    fn check(&self, db: &Connection) -> Result<()> {
        ensure!(
            self.available.load(Ordering::Acquire),
            "fixture source process-refused"
        );
        self.identity(db)
    }

    fn bootstrap(&self) -> Result<()> {
        let page = self.read(|db| {
            self.check(db)?;
            self.installer.prepare_scan(
                db,
                &ScanPage {
                    job: self.manifest.job.clone(),
                    expected_cursor: vec![],
                    next_cursor: vec![],
                    position: self.installer.position(db, SOURCE)?,
                    rows: vec![],
                    finished: true,
                },
                limits(),
            )
        })?;
        self.publish(&page)?;
        let mut page = self.read(|db| {
            self.check(db)?;
            let mut page = self
                .installer
                .prepare_catch_up(db, &self.manifest.job, limits())?;
            page.capture_table(db, "slice_coverage")?;
            Ok(page)
        })?;
        coverage(&mut page)?;
        ensure!(
            self.publish(&page)? == Outcome::Published,
            "empty bootstrap did not publish"
        );
        Ok(())
    }

    fn capture(&self) -> Result<OwnedPage> {
        self.capture_with_inputs(PAGE_ROWS, PAGE_ROWS * REFERENCE_BYTES)
    }

    fn capture_with_inputs(&self, input_rows: usize, input_bytes: usize) -> Result<OwnedPage> {
        self.read(|db| {
            self.check(db)?;
            let mut page=self.installer.prepare_live_bounded(db,VIEW,limits(),input_rows,input_bytes)?;
            ensure!(page.rows().len()<=PAGE_ROWS,"fixture input fanout exceeded");
            page.capture_table(db,"slice_output")?;
            page.capture_table(db,"slice_coverage")?;
            let mut images=Vec::new();
            let mut bytes=0usize;
            for row in page.rows() {
                smallclaims::read_budget::check()?;
                let reference=row.new.as_ref().context("fixture input has no immutable new image")?;
                ensure!(row.old.is_none() && reference["table"]=="claims" && reference["side"]==1,
                    "fixture unsupported source mutation");
                let revision=reference["revision"].as_u64().context("fixture malformed revision")?;
                let key:Value=serde_json::from_str(&row.key)?;
                let expected_index=key[1][0].as_u64().context("fixture malformed primary key")?;
                ensure!(key[0]=="claims" && expected_index>0,"fixture unexpected key");
                // Direct columns only; refuse before body decoding/copying. This does not
                // qualify SQLite overflow/index/metadata work or malformed physical schemas.
                let (id,subject,kind,body,recorded):(i64,i64,i64,i64,i64)=db.query_row(
                    "SELECT octet_length(claim_id),octet_length(subject),octet_length(kind),octet_length(body),bytes
                     FROM main.slice_images WHERE revision=?1", [revision],
                    |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)),
                )?;
                ensure!((1..=128).contains(&id) && (0..=256).contains(&subject) && (0..=128).contains(&kind)
                    && body>=0 && 8+id+subject+kind+body==recorded && recorded<=IMAGE_BYTES as i64,
                    "fixture image width/counter invalid");
                bytes=bytes.checked_add(recorded as usize).context("fixture page bytes overflow")?;
                ensure!(bytes<=PAGE_BYTES,"fixture page bytes exceeded");
                let image=db.query_row(
                    "SELECT store_index,claim_id,subject,kind,body FROM main.slice_images WHERE revision=?1",[revision],
                    |r|Ok(Image{store_index:r.get(0)?,id:r.get(1)?,subject:r.get(2)?,kind:r.get(3)?,body:r.get(4)?}),
                )?;
                ensure!(image.store_index==expected_index,"fixture image/reference mismatch");
                images.push(image);
            }
            self.check(db)?;
            Ok(OwnedPage{prepared:page,images,bytes})
        })
    }

    fn publish(&self, page: &PreparedPage) -> Result<Outcome> {
        smallclaims::read_budget::check()?;
        let mut writer = self.store.connection.write_background();
        smallclaims::read_budget::check()?;
        let tx = writer.transaction()?;
        self.check(&tx)?;
        // Propagate any helper failure: no caller catch-and-commit path is offered here.
        // Unsafe cleanup and hook provenance still need the independent managed exit contract.
        let outcome = self.installer.publish_prepared(&tx, page, 1)?;
        smallclaims::read_budget::check()?;
        tx.commit()?;
        Ok(outcome)
    }

    fn ready_rows(&self) -> Result<Vec<(String, String, String, String)>> {
        self.read(|db| {
            self.check(db)?;
            let status=self.installer.status(db,VIEW)?;
            ensure!(status.ready,"fixture output is pending or unavailable");
            let root=self.installer.root(db,VIEW)?;
            let through:u64=db.query_row("SELECT through FROM main.slice_coverage WHERE namespace=?1",
                [root.namespace.as_str()],|r|r.get(0))?;
            ensure!(through==status.source.revision,"fixture output coverage mismatched");
            let mut statement=db.prepare("SELECT claim_id,subject,kind,digest FROM main.slice_output WHERE namespace=?1 ORDER BY claim_id LIMIT 4097")?;
            let rows=statement.query_map([root.namespace.as_str()],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            ensure!(rows.len()<=TOTAL_ROWS as usize,"fixture output retention exceeded");
            Ok(rows)
        })
    }

    fn tuple(&self) -> Result<(u64, u64, u64, u64, u64)> {
        self.read(|db| {
            Ok((
                db.query_row(
                    "SELECT revision FROM main.ivm_install_sources WHERE name=?1",
                    [SOURCE],
                    |r| r.get(0),
                )?,
                db.query_row(
                    "SELECT revision FROM main.ivm_install_roots WHERE view=?1",
                    [VIEW],
                    |r| r.get(0),
                )?,
                db.query_row(
                    "SELECT journal_rows FROM main.ivm_install_sources WHERE name=?1",
                    [SOURCE],
                    |r| r.get(0),
                )?,
                db.query_row(
                    "SELECT retained_rows FROM main.slice_limits WHERE id=1",
                    [],
                    |r| r.get(0),
                )?,
                db.query_row("SELECT COUNT(*) FROM main.slice_output", [], |r| r.get(0))?,
            ))
        })
    }

    fn prune(&self) -> Result<usize> {
        let mut writer = self.store.connection.write_background();
        let tx = writer.transaction()?;
        self.check(&tx)?;
        let applied: u64 = tx.query_row(
            "SELECT revision FROM main.ivm_install_roots WHERE view=?1",
            [VIEW],
            |r| r.get(0),
        )?;
        self.installer.prune_journal(&tx, SOURCE, PAGE_ROWS)?;
        tx.execute("UPDATE main.slice_limits SET draining=1 WHERE id=1", [])?;
        let count = tx.execute(
            "DELETE FROM main.slice_images WHERE revision IN
            (SELECT revision FROM main.slice_images WHERE revision<=?1 ORDER BY revision LIMIT ?2)",
            params![applied, PAGE_ROWS],
        )?;
        tx.execute("UPDATE main.slice_limits SET draining=0 WHERE id=1", [])?;
        tx.commit()?;
        Ok(count)
    }
}

fn coverage(page: &mut PreparedPage) -> Result<()> {
    let through = page.position().revision as i64;
    page.upsert("slice_coverage", vec![Sql::Integer(through)])?;
    page.require_row(
        "slice_coverage",
        vec![],
        vec![("through".into(), Sql::Integer(through))],
    )?;
    Ok(())
}

impl OwnedPage {
    fn reduce(mut self) -> Result<PreparedPage> {
        smallclaims::read_budget::check()?;
        ensure!(
            self.images.len() <= PAGE_ROWS && self.bytes <= PAGE_BYTES,
            "fixture owned page exceeds bounds"
        );
        for image in self.images {
            smallclaims::read_budget::check()?;
            let digest = hex::encode(Sha256::digest(&image.body));
            ensure!(
                image.id.len() + image.subject.len() + image.kind.len() + digest.len() <= 512,
                "fixture output width exceeded"
            );
            self.prepared.upsert(
                "slice_output",
                vec![
                    Sql::Text(image.id),
                    Sql::Text(image.subject),
                    Sql::Text(image.kind),
                    Sql::Text(digest),
                ],
            )?;
            self.prepared.mark_visible_change();
        }
        smallclaims::read_budget::check()?;
        coverage(&mut self.prepared)?;
        Ok(self.prepared)
    }
}

fn input(subject: &str, body: &str) -> ClaimInput {
    ClaimInput {
        subject: subject.into(),
        kind: "example.slice-unknown".into(),
        actor: None,
        fields: BTreeMap::from([("content".into(), json!(body))]),
        evidence: vec![],
        expected_subject: None,
        idempotency_key: None,
    }
}
fn append(slice: &Slice, subject: &str) -> Result<ClaimRecord> {
    Ok(slice.store.append_claim(&input(subject, "small"))?)
}
fn fixture() -> Result<(tempfile::TempDir, Slice)> {
    let root = tempfile::tempdir()?;
    let slice = Slice::create(&root.path().join("claims.db"))?;
    Ok((root, slice))
}

#[test]
fn native_outer_and_savepoint_rollback_restore_claim_image_reference_and_prefix() -> Result<()> {
    let (_root, slice) = fixture()?;
    let before = slice.tuple()?;
    {
        let mut writer = slice.store.connection.write();
        let tx = writer.transaction()?;
        Plain.append_claim_tx(
            &tx,
            "slice-fixture",
            "note/rollback",
            "example.slice-unknown",
            None,
            &json!({"fields":{"content":"rollback"}}),
            &[],
            None,
        )?;
        assert_eq!(slice.installer.position(&tx, SOURCE)?.revision, 1);
        tx.rollback()?;
    }
    assert_eq!(slice.tuple()?, before);
    assert!(slice.store.claims_for("note/rollback", None)?.is_empty());
    {
        let mut writer = slice.store.connection.write();
        let mut tx = writer.transaction()?;
        let mut savepoint = tx.savepoint()?;
        Plain.append_claim_tx(
            &savepoint,
            "slice-fixture",
            "note/savepoint",
            "example.slice-unknown",
            None,
            &json!({"fields":{}}),
            &[],
            None,
        )?;
        savepoint.rollback()?;
        savepoint.commit()?;
        tx.commit()?;
    }
    assert_eq!(slice.tuple()?, before);
    assert!(slice.ready_rows()?.is_empty());
    Ok(())
}

#[test]
fn owned_page_newer_input_foreground_interleave_and_consumed_prefix_reclamation() -> Result<()> {
    let (_root, slice) = fixture()?;
    let first = append(&slice, "note/first")?;
    assert!(slice.ready_rows().is_err());
    let owned = slice.capture()?;
    let second = append(&slice, "note/second")?; // no reader/writer retained by owned page
    let page = owned.reduce()?;
    slice.publish(&page)?;
    assert_eq!(slice.tuple()?, (2, 1, 2, 2, 1));
    assert!(slice.ready_rows().is_err());
    assert_eq!(slice.prune()?, 1);
    assert_eq!(slice.tuple()?, (2, 1, 1, 1, 1));
    let third = append(&slice, "note/third")?; // a real foreground Store append between pages
    let page = slice.capture()?.reduce()?;
    slice.publish(&page)?;
    let rows = slice.ready_rows()?;
    let mut expected = vec![first.id, second.id, third.id];
    expected.sort();
    assert_eq!(
        rows.iter().map(|r| r.0.clone()).collect::<Vec<_>>(),
        expected
    );
    for row in rows {
        let claim = slice
            .store
            .claims_for(&row.1, None)?
            .into_iter()
            .find(|c| c.id == row.0)
            .unwrap();
        assert_eq!(row.2, claim.kind);
        assert_eq!(
            row.3,
            hex::encode(Sha256::digest(
                smallclaims::hash::canonical_json_text(&claim.body)?.as_bytes()
            ))
        );
    }
    assert!(
        slice.publish(&page).is_err(),
        "a stale prepared prefix must not double-apply"
    );
    assert_eq!(slice.tuple()?.4, 3);
    Ok(())
}

#[test]
fn cumulative_reference_bytes_stop_before_next_input_and_exact_retry_preserves_old_budget()
-> Result<()> {
    let (_root, slice) = fixture()?;
    append(&slice, "note/byte-first")?;
    append(&slice, "note/byte-second")?;
    let sizes = slice.read(|db| {
        let mut statement = db.prepare(
            "SELECT bytes FROM main.ivm_install_journal WHERE source=?1 ORDER BY revision LIMIT 2",
        )?;
        Ok(statement
            .query_map([SOURCE], |r| r.get::<_, usize>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    })?;
    assert_eq!(sizes.len(), 2);
    assert!(sizes[0] > 1 && sizes[1] <= sizes[0] && sizes[0] <= REFERENCE_BYTES);
    let before = slice.tuple()?;
    assert!(slice.capture_with_inputs(PAGE_ROWS, sizes[0] - 1).is_err());
    assert_eq!(slice.tuple()?, before);
    slice.read(|db| {
        slice.check(db)?;
        let mut old = slice.installer.prepare_live(
            db,
            VIEW,
            PublicationLimits {
                rows: PAGE_ROWS,
                bytes: sizes[0],
                tables: 2,
            },
        )?;
        assert_eq!(old.rows().len(), 1);
        assert_eq!(old.position().revision, 1);
        old.capture_table(db, "slice_output")?;
        assert!(
            old.upsert(
                "slice_output",
                vec![
                    Sql::Text("id".into()),
                    Sql::Text("subject".into()),
                    Sql::Text("kind".into()),
                    Sql::Text("digest".into())
                ]
            )
            .is_err(),
            "original prepare_live must still charge references to its combined byte budget"
        );
        assert_eq!(
            slice
                .installer
                .prepare_live(db, VIEW, limits())?
                .rows()
                .len(),
            2
        );
        Ok(())
    })?;
    let owned = slice.capture_with_inputs(PAGE_ROWS, sizes[0])?;
    assert_eq!(owned.images.len(), 1);
    assert_eq!(owned.prepared.position().revision, 1);
    let page = owned.reduce()?;
    slice.publish(&page)?;
    assert_eq!(slice.tuple()?, (2, 1, 2, 2, 1));
    assert!(slice.ready_rows().is_err());
    let after_first = slice.tuple()?;
    assert!(slice.publish(&page).is_err());
    assert_eq!(slice.tuple()?, after_first);
    let second = slice.capture_with_inputs(PAGE_ROWS, sizes[1])?;
    assert_eq!(second.images.len(), 1);
    assert_eq!(second.prepared.position().revision, 2);
    slice.publish(&second.reduce()?)?;
    assert_eq!(slice.ready_rows()?.len(), 2);
    Ok(())
}

#[test]
fn witnessed_output_dml_then_coverage_failure_rolls_back_before_prefix_and_exact_retry()
-> Result<()> {
    use rusqlite::hooks::{Action, AuthAction, AuthContext, Authorization};
    let (_root, slice) = fixture()?;
    append(&slice, "note/in-turn-failure")?;
    let page = slice.capture()?.reduce()?;
    let before = slice.tuple()?;
    let wrote_output = Arc::new(AtomicBool::new(false));
    let refused_coverage = Arc::new(AtomicBool::new(false));
    {
        let mut writer = slice.store.connection.write_background();
        // Plain supplies no WriterObserver/authorizer. These hooks belong only to this
        // fixture fault, do no SQL, and are removed before returning the loan.
        let wrote = wrote_output.clone();
        writer.update_hook(Some(
            move |action: Action, database: &str, table: &str, _| {
                if action == Action::SQLITE_INSERT && database == "main" && table == "slice_output"
                {
                    wrote.store(true, Ordering::Release);
                }
            },
        ));
        let wrote = wrote_output.clone();
        let refused = refused_coverage.clone();
        writer.authorizer(Some(move |cx: AuthContext<'_>| {
            if matches!(
                cx.action,
                AuthAction::Insert {
                    table_name: "slice_coverage"
                }
            ) && cx.database_name == Some("main")
                && wrote.load(Ordering::Acquire)
            {
                refused.store(true, Ordering::Release);
                Authorization::Deny
            } else {
                Authorization::Allow
            }
        }));
        let result = (|| -> Result<()> {
            let tx = writer.transaction()?;
            slice.installer.publish_prepared(&tx, &page, 1)?;
            tx.commit()?;
            Ok(())
        })();
        writer.authorizer(None::<fn(AuthContext<'_>) -> Authorization>);
        writer.update_hook(None::<fn(Action, &str, &str, i64)>);
        assert!(result.is_err());
        assert!(
            writer.is_autocommit(),
            "fault must resolve the fixture outer transaction"
        );
    }
    assert!(
        wrote_output.load(Ordering::Acquire),
        "outside-SQL witness must observe actual output DML"
    );
    assert!(
        refused_coverage.load(Ordering::Acquire),
        "fault must occur after that DML and before coverage/P"
    );
    assert_eq!(slice.tuple()?, before);
    assert!(slice.ready_rows().is_err());
    slice.read(|db| {
        assert_eq!(
            db.query_row(
                "SELECT COUNT(*) FROM main.slice_coverage WHERE through<>0",
                [],
                |r| r.get::<_, u64>(0)
            )?,
            0
        );
        Ok(())
    })?;
    slice.publish(&page)?;
    assert_eq!(slice.tuple()?, (1, 1, 1, 1, 1));
    assert_eq!(slice.ready_rows()?.len(), 1);
    Ok(())
}

#[test]
fn page_outer_rollback_and_refused_commit_leave_output_and_coverage_atomic() -> Result<()> {
    let (_root, slice) = fixture()?;
    append(&slice, "note/atomic")?;
    let page = slice.capture()?.reduce()?;
    let before = slice.tuple()?;
    {
        let mut writer = slice.store.connection.write_background();
        let tx = writer.transaction()?;
        slice.installer.publish_prepared(&tx, &page, 1)?;
        assert_eq!(
            tx.query_row("SELECT COUNT(*) FROM main.slice_output", [], |r| r
                .get::<_, u64>(0))?,
            1
        );
        assert_eq!(
            slice.tuple()?,
            before,
            "another WAL reader must not see uncommitted output/frontier"
        );
        tx.rollback()?;
    }
    assert_eq!(slice.tuple()?, before);
    assert!(slice.ready_rows().is_err());
    {
        let mut writer = slice.store.connection.write_background();
        // Fixture owns this otherwise unconfigured native commit hook; no production hook
        // replacement is proposed. Remove it before releasing the same writer loan.
        writer.commit_hook(Some(|| true));
        let result = (|| -> Result<()> {
            let tx = writer.transaction()?;
            slice.installer.publish_prepared(&tx, &page, 1)?;
            tx.commit()?;
            Ok(())
        })();
        writer.commit_hook(None::<fn() -> bool>);
        assert!(result.is_err());
        assert!(
            writer.is_autocommit(),
            "refused outer commit must leave an idle fixture writer"
        );
    }
    assert_eq!(slice.tuple()?, before);
    assert!(slice.ready_rows().is_err());
    slice.publish(&page)?;
    assert_eq!(slice.ready_rows()?.len(), 1);
    Ok(())
}

#[test]
fn overwidth_and_queue_exhaustion_preserve_native_admission_but_refuse_coverage() -> Result<()> {
    let (_root, slice) = fixture()?;
    let claim = slice
        .store
        .append_claim(&input("note/wide", &"x".repeat(IMAGE_BYTES)))?;
    assert_eq!(slice.store.claims_for("note/wide", None)?[0].id, claim.id);
    assert!(slice.capture().is_err());
    assert!(slice.ready_rows().is_err());
    assert_eq!(
        slice.tuple()?.3,
        0,
        "no oversized immutable image may be copied"
    );
    let (_other, full) = fixture()?;
    for n in 0..PENDING_ROWS {
        append(&full, &format!("note/{n}"))?;
    }
    assert_eq!(full.tuple()?.3, PENDING_ROWS);
    let extra = append(&full, "note/overflow")?;
    assert_eq!(
        full.store.claims_for("note/overflow", None)?[0].id,
        extra.id
    );
    assert_eq!(full.tuple()?.3, PENDING_ROWS);
    assert!(full.capture().is_err());
    assert!(full.ready_rows().is_err());
    Ok(())
}

#[test]
fn page_row_bound_and_missing_image_never_skip_to_a_ready_prefix() -> Result<()> {
    let (_root, slice) = fixture()?;
    slice.read(|db| {
        slice.check(db)?;
        for (rows, bytes) in [
            (0, 1),
            (1, 0),
            (limits().rows + 1, 1),
            (1, limits().bytes + 1),
        ] {
            assert!(
                slice
                    .installer
                    .prepare_live_bounded(db, VIEW, limits(), rows, bytes)
                    .is_err()
            );
        }
        Ok(())
    })?;
    for n in 0..=PAGE_ROWS {
        append(&slice, &format!("note/page-{n}"))?;
    }
    let owned = slice.capture()?;
    assert_eq!(owned.images.len(), PAGE_ROWS);
    assert!(owned.bytes <= PAGE_BYTES);
    slice.publish(&owned.reduce()?)?;
    assert_eq!(slice.tuple()?.1, PAGE_ROWS as u64);
    assert!(slice.ready_rows().is_err());
    slice
        .store
        .connection
        .batched(|tx| {
            tx.execute(
                "DELETE FROM main.slice_images WHERE revision=?1",
                [PAGE_ROWS as u64 + 1],
            )?;
            Ok::<_, anyhow::Error>(())
        })
        .map_err(anyhow::Error::msg)??;
    assert!(slice.capture().is_err());
    assert!(slice.ready_rows().is_err());
    Ok(())
}

#[test]
fn repair_delete_identity_schema_and_copied_file_refuse_old_pages() -> Result<()> {
    let (root, slice) = fixture()?;
    let claim = append(&slice, "note/repaired")?;
    let page = slice.capture()?.reduce()?;
    slice
        .store
        .connection
        .batched(|tx| {
            tx.execute("UPDATE main.claims SET body='{}' WHERE id=?1", [&claim.id])?;
            Ok::<_, anyhow::Error>(())
        })
        .map_err(anyhow::Error::msg)??;
    assert!(slice.publish(&page).is_err());
    assert!(slice.ready_rows().is_err());
    let (_replace_root, replace) = fixture()?;
    let original = append(&replace, "note/replaced")?;
    let old_page = replace.capture()?.reduce()?;
    replace.store.connection.batched(|tx| {
        tx.execute("INSERT OR REPLACE INTO main.claims(id,batch_id,subject,kind,origin,actor,body,predecessors,accepted_at_unix_ms)
            SELECT id,batch_id,subject,kind,origin,actor,body,predecessors,accepted_at_unix_ms FROM main.claims WHERE id=?1", [&original.id])?;
        Ok::<_, anyhow::Error>(())
    }).map_err(anyhow::Error::msg)??;
    assert_eq!(
        replace.store.claims_for("note/replaced", None)?[0].id,
        original.id
    );
    assert!(replace.publish(&old_page).is_err());
    assert!(replace.ready_rows().is_err());
    let (_index_root, index_replace) = fixture()?;
    let indexed = append(&index_replace, "note/index-replaced")?;
    let indexed_page = index_replace.capture()?.reduce()?;
    let retained_before = index_replace.read(|db| {
        Ok(db.query_row(
            "SELECT retained_rows,retained_bytes,total_rows FROM main.slice_limits WHERE id=1",
            [],
            |r| {
                Ok((
                    r.get::<_, u64>(0)?,
                    r.get::<_, u64>(1)?,
                    r.get::<_, u64>(2)?,
                ))
            },
        )?)
    })?;
    let new_id = "fixture-index-replacement";
    index_replace.store.connection.batched(|tx| {
        tx.execute("INSERT OR REPLACE INTO main.claims(store_index,id,batch_id,subject,kind,origin,actor,body,predecessors,accepted_at_unix_ms)
            SELECT store_index,?1,batch_id,subject,kind,origin,actor,body,predecessors,accepted_at_unix_ms FROM main.claims WHERE id=?2",
            params![new_id, indexed.id])?;
        Ok::<_, anyhow::Error>(())
    }).map_err(anyhow::Error::msg)??;
    assert_eq!(
        index_replace
            .store
            .claims_for("note/index-replaced", None)?[0]
            .id,
        new_id
    );
    index_replace.read(|db| {
        assert_eq!(
            db.query_row(
                "SELECT claim_id FROM main.slice_images WHERE store_index=?1",
                [indexed.store_index],
                |r| r.get::<_, String>(0)
            )?,
            indexed.id
        );
        assert_eq!(
            db.query_row(
                "SELECT retained_rows,retained_bytes,total_rows FROM main.slice_limits WHERE id=1",
                [],
                |r| Ok((
                    r.get::<_, u64>(0)?,
                    r.get::<_, u64>(1)?,
                    r.get::<_, u64>(2)?
                ))
            )?,
            retained_before
        );
        assert!(!db.query_row(
            "SELECT available FROM main.ivm_install_sources WHERE name=?1",
            [SOURCE],
            |r| r.get::<_, bool>(0)
        )?);
        Ok(())
    })?;
    assert!(index_replace.capture().is_err());
    assert!(index_replace.publish(&indexed_page).is_err());
    assert!(index_replace.ready_rows().is_err());
    let (_other, replaced) = fixture()?;
    append(&replaced, "note/epoch")?;
    let old = replaced.capture()?.reduce()?;
    replaced
        .store
        .connection
        .batched(|tx| {
            tx.execute(
                "UPDATE main.ivm_install_sources SET epoch=epoch+1 WHERE name=?1",
                [SOURCE],
            )?;
            Ok::<_, anyhow::Error>(())
        })
        .map_err(anyhow::Error::msg)??;
    assert!(replaced.publish(&old).is_err());
    assert!(replaced.ready_rows().is_err());
    let (_deleted, deleted) = fixture()?;
    let removed = append(&deleted, "note/deleted")?;
    deleted.publish(&deleted.capture()?.reduce()?)?;
    deleted
        .store
        .connection
        .batched(|tx| {
            tx.execute("DELETE FROM main.claims WHERE id=?1", [&removed.id])?;
            Ok::<_, anyhow::Error>(())
        })
        .map_err(anyhow::Error::msg)??;
    assert!(deleted.ready_rows().is_err());
    let (_ddl, changed) = fixture()?;
    changed
        .store
        .connection
        .batched(|tx| {
            tx.execute_batch("CREATE TABLE main.slice_unrelated_ddl(id INTEGER PRIMARY KEY)")?;
            Ok::<_, anyhow::Error>(())
        })
        .map_err(anyhow::Error::msg)??;
    assert!(changed.ready_rows().is_err());
    // Private-copy identity is independent of copied Installer metadata. No Core copy/open
    // hook is implied: only this fixture's closed gate refuses the destination's getters.
    let (_lifetime, origin) = fixture()?;
    append(&origin, "note/copy")?;
    origin.publish(&origin.capture()?.reduce()?)?;
    assert_eq!(origin.ready_rows()?.len(), 1);
    let copy = root.path().join("copy.db");
    {
        let writer = origin.store.connection.write_background();
        writer.execute(
            "VACUUM main INTO ?1",
            [copy.to_str().context("fixture non-UTF8 path")?],
        )?;
    }
    std::fs::copy(manifest_path(&origin.path), manifest_path(&copy))?;
    let copied = Slice::open(&copy)?;
    assert!(
        copied.store.readers.get().query_row(
            "SELECT available FROM main.ivm_install_sources WHERE name=?1",
            [SOURCE],
            |r| r.get::<_, bool>(0)
        )?,
        "a copied available bit must not authorize the destination lifetime"
    );
    assert!(!copied.available.load(Ordering::Acquire));
    assert!(copied.capture().is_err());
    assert!(copied.ready_rows().is_err());
    Ok(())
}

#[test]
fn duplicate_native_insert_and_exact_image_byte_boundary_do_not_invent_coverage() -> Result<()> {
    let (_root, slice) = fixture()?;
    let first = append(&slice, "note/duplicate")?;
    let before = slice.tuple()?;
    slice.store.connection.batched(|tx| {
        assert_eq!(tx.execute("INSERT OR IGNORE INTO main.claims(id,batch_id,subject,kind,origin,actor,body,predecessors,accepted_at_unix_ms)
            SELECT id,batch_id,subject,kind,origin,actor,body,predecessors,accepted_at_unix_ms FROM main.claims WHERE id=?1",[&first.id])?,0);
        Ok::<_,anyhow::Error>(())
    }).map_err(anyhow::Error::msg)??;
    assert_eq!(slice.tuple()?, before);
    let subject = "note/exact-width";
    let empty = input(subject, "");
    let body = smallclaims::hash::canonical_json_text(
        &json!({"fields":empty.fields,"evidence":empty.evidence}),
    )?;
    let padding = IMAGE_BYTES - 8 - first.id.len() - subject.len() - empty.kind.len() - body.len();
    let exact = slice
        .store
        .append_claim(&input(subject, &"x".repeat(padding)))?;
    assert_eq!(
        slice.read(|db| Ok(db.query_row(
            "SELECT bytes FROM main.slice_images WHERE claim_id=?1",
            [&exact.id],
            |r| r.get::<_, usize>(0)
        )?))?,
        IMAGE_BYTES
    );
    let wide = slice
        .store
        .append_claim(&input(subject, &"x".repeat(padding + 1)))?;
    assert!(
        slice
            .store
            .claims_for(subject, None)?
            .iter()
            .any(|claim| claim.id == wide.id)
    );
    assert!(slice.ready_rows().is_err());
    assert_eq!(slice.tuple()?.3, 2);
    Ok(())
}

#[test]
fn reader_rollback_refusal_closes_exact_connection_and_clean_panic_returns_idle_reader()
-> Result<()> {
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization, TransactionOperation};
    let (_root, slice) = fixture()?;
    let opened = slice.store.readers.usage().opened;
    let result = slice.read(|db| {
        db.query_row(
            "SELECT revision FROM main.ivm_install_sources WHERE name=?1",
            [SOURCE],
            |r| r.get::<_, u64>(0),
        )?;
        db.authorizer(Some(|cx: AuthContext<'_>| match cx.action {
            AuthAction::Transaction {
                operation: TransactionOperation::Rollback,
            } => Authorization::Deny,
            _ => Authorization::Allow,
        }));
        Ok(7)
    });
    assert!(result.is_err());
    assert!(!slice.available.load(Ordering::Acquire));
    {
        let fresh = slice.store.readers.try_get()?;
        assert!(fresh.is_autocommit());
        assert!(
            slice.store.readers.usage().opened > opened,
            "poisoned exact reader must not be cached"
        );
    }
    let (_clean, clean) = fixture()?;
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _: Result<()> = clean.read(|db| {
                db.query_row(
                    "SELECT revision FROM main.ivm_install_sources WHERE name=?1",
                    [SOURCE],
                    |r| r.get::<_, u64>(0),
                )?;
                panic!("fixture owned capture panic");
            });
        }))
        .is_err()
    );
    assert!(clean.available.load(Ordering::Acquire));
    assert!(clean.ready_rows()?.is_empty());
    Ok(())
}

#[test]
fn cancellation_refuses_owned_capture_reduction_and_publication_without_advancing_output()
-> Result<()> {
    use smallclaims::read_budget::{ReadBudget, with};
    use std::time::Duration;
    let (_root, slice) = fixture()?;
    append(&slice, "note/cancel")?;
    let before = slice.tuple()?;
    let parent = ReadBudget::new("fixture/slice", Duration::from_secs(30));
    let child = parent.child(Duration::from_secs(15));
    let captured = with(Some(child), || {
        slice.read(|db| {
            slice.check(db)?;
            parent.cancel();
            Ok(7)
        })
    });
    assert!(
        captured.is_err(),
        "cancellation after callback must refuse its result"
    );
    assert!(slice.available.load(Ordering::Acquire));
    assert_eq!(slice.tuple()?, before); // cleanup removed cancellation and returned an idle reader
    let owned = slice.capture()?;
    assert!(with(Some(parent.clone()), || owned.reduce()).is_err());
    let page = slice.capture()?.reduce()?;
    assert!(with(Some(parent), || slice.publish(&page)).is_err());
    assert_eq!(slice.tuple()?, before);
    slice.publish(&page)?;
    assert_eq!(slice.ready_rows()?.len(), 1);
    Ok(())
}

#[test]
fn compatible_store_reopen_resumes_persisted_prefix_without_open_replay() -> Result<()> {
    let (root, slice) = fixture()?;
    append(&slice, "note/restart-first")?;
    slice.publish(&slice.capture()?.reduce()?)?;
    append(&slice, "note/restart-pending")?;
    let before = slice.tuple()?;
    drop(slice);
    let reopened = Slice::open(&root.path().join("claims.db"))?;
    assert!(reopened.available.load(Ordering::Acquire));
    assert_eq!(
        reopened.tuple()?,
        before,
        "open must not apply/rebuild pending output"
    );
    assert!(reopened.ready_rows().is_err());
    reopened.publish(&reopened.capture()?.reduce()?)?;
    assert_eq!(reopened.ready_rows()?.len(), 2);
    Ok(())
}

#[test]
fn real_store_crash_child() -> Result<()> {
    let Some(phase) = std::env::var_os("SLICE_FIXTURE_CHILD_PHASE") else {
        return Ok(());
    };
    let path = PathBuf::from(
        std::env::var_os("SLICE_FIXTURE_CHILD_PATH").context("child fixture path missing")?,
    );
    let slice = Slice::create(&path)?;
    append(&slice, "note/crash")?;
    let code = match phase.to_str().context("child phase invalid")? {
        "admitted" => 73,
        "uncommitted-output" => {
            let page = slice.capture()?.reduce()?;
            let mut writer = slice.store.connection.write_background();
            let tx = writer.transaction()?;
            slice.installer.publish_prepared(&tx, &page, 1)?;
            println!("SLICE_CRASH uncommitted-output");
            std::io::stdout().flush()?;
            std::process::exit(74); // actual child death, no transaction destructors
        }
        "committed-output" => {
            slice.publish(&slice.capture()?.reduce()?)?;
            75
        }
        _ => bail!("unknown fixture child phase"),
    };
    println!("SLICE_CRASH {}", phase.to_string_lossy());
    std::io::stdout().flush()?;
    std::process::exit(code);
}

#[test]
fn actual_child_crashes_resume_only_durable_admitted_and_applied_prefixes() -> Result<()> {
    for (phase, code, applied) in [
        ("admitted", 73, 0),
        ("uncommitted-output", 74, 0),
        ("committed-output", 75, 1),
    ] {
        let root = tempfile::tempdir()?;
        let path = root.path().join("claims.db");
        let output = Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "real_store_crash_child",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("SLICE_FIXTURE_CHILD_PHASE", phase)
            .env("SLICE_FIXTURE_CHILD_PATH", &path)
            .output()?;
        assert_eq!(
            output.status.code(),
            Some(code),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains(&format!("SLICE_CRASH {phase}")));
        let reopened = Slice::open(&path)?;
        assert!(reopened.available.load(Ordering::Acquire));
        assert_eq!(reopened.tuple()?, (1, applied, 1, 1, applied));
        if applied == 0 {
            assert!(reopened.ready_rows().is_err());
            reopened.publish(&reopened.capture()?.reduce()?)?;
        } else {
            assert!(
                reopened.capture()?.images.is_empty(),
                "committed page must not reappear after process death"
            );
        }
        assert_eq!(reopened.ready_rows()?.len(), 1);
        assert_eq!(reopened.prune()?, 1);
        assert_eq!(reopened.tuple()?, (1, 1, 0, 0, 1));
    }
    Ok(())
}

// An explicit accounting prerequisite, not a silent zero-count measurement when the
// library is built without test-support. This does not grant a new build/load turn.
#[cfg(feature = "test-support")]
#[test]
fn full_page_accounting_includes_metadata_and_physical_reader_writer_return() -> Result<()> {
    let (_root, slice) = fixture()?;
    for n in 0..PAGE_ROWS {
        append(&slice, &format!("note/cost-{n}"))?;
    }
    let scope = smallclaims::sqlite::work::SqliteWorkScope::start();
    let page = slice.capture()?.reduce()?;
    slice.publish(&page)?;
    let work = scope.finish(); // includes both guard returns, before oracle queries
    assert!(
        work.statements > 0,
        "zero traced statements is not qualified work evidence"
    );
    assert!(
        work.statements <= 128,
        "unqualified page statement work: {work:?}"
    );
    assert!(work.vm_steps <= 50000, "unqualified page VM work: {work:?}");
    assert_eq!(
        work.autoindex_rows, 0,
        "page must not construct an automatic index"
    );
    assert_eq!(slice.ready_rows()?.len(), PAGE_ROWS);
    Ok(())
}

#[cfg(not(feature = "test-support"))]
#[test]
fn full_page_accounting_includes_metadata_and_physical_reader_writer_return() {
    panic!(
        "required page-work accounting is unavailable: build this target with --features test-support and require the named control with a positive statement count; missing instrumentation is not a passed or omitted prerequisite"
    );
}
