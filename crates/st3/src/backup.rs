//! Versioned logical backups. Envelope payloads use the peer wire format unchanged.
use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::client::Endpoint;
use crate::model::{ReplicaEnvelope, ReplicaEnvelopeSignature};
use crate::store::{CheckpointManifest, CheckpointManifestPage, Store};

pub const FORMAT: &str = "st3.claim-backup";
pub const VERSION: u32 = 1;
pub(crate) const PAGE: usize = 512;
// One envelope can contain document bytes; reject unbounded lines before allocating them.
const MAX_RECORD_BYTES: u64 = 128 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct TableDigest {
    pub digest: String,
    pub count: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Header {
    pub format: String,
    pub version: u32,
    pub source_build: String,
    pub source_schema: u32,
    pub registry_digest: String,
    pub fleet_id: Option<String>,
    pub fleet_anchor: Option<String>,
    pub graph_digest: String,
    pub log_digest: String,
    pub tables: BTreeMap<String, TableDigest>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "record", content = "data", rename_all = "kebab-case")]
pub enum Record {
    Header(Header),
    Envelope(ReplicaEnvelope),
    Checkpoint(CheckpointManifestPage),
    Signatures(Vec<ReplicaEnvelopeSignature>),
    End { envelopes: u64, sha256: String },
}

#[derive(Debug, Deserialize, Serialize)]
pub struct RestoreReport {
    pub writer: String,
    pub envelopes: u64,
    pub graph_digest: String,
    pub source_graph_digest: String,
    pub log_digest: String,
    pub projections_match: bool,
    pub tables: BTreeMap<String, TableDigest>,
}

pub(crate) fn write_record(
    output: &mut impl Write,
    hash: &mut Sha256,
    record: &Record,
) -> Result<()> {
    let mut bytes = serde_json::to_vec(record)?;
    bytes.push(b'\n');
    ensure!(
        bytes.len() as u64 <= MAX_RECORD_BYTES,
        "backup record exceeds the format size limit"
    );
    output.write_all(&bytes)?;
    hash.update(&bytes);
    Ok(())
}

/// Download to a private temporary file, then publish only a complete, checksummed archive.
pub async fn create(endpoint: &Endpoint, destination: &Path) -> Result<()> {
    use futures_util::StreamExt;
    use tokio::io::AsyncWriteExt;
    ensure!(!destination.exists(), "backup file already exists");
    let directory = destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let temporary = tempfile::NamedTempFile::new_in(directory)?;
    let mut output = tokio::fs::File::from_std(temporary.reopen()?);
    let (client, url) = match endpoint {
        Endpoint::Unix(socket) => (
            reqwest::Client::builder()
                .unix_socket(socket.as_path())
                .build()?,
            "http://localhost/v1/backup".to_owned(),
        ),
        Endpoint::Http(base) => (reqwest::Client::new(), format!("{base}/v1/backup")),
    };
    let response = client.get(url).send().await?.error_for_status()?;
    let mut stream = response.bytes_stream();
    while let Some(bytes) = stream.next().await {
        output.write_all(&bytes?).await?;
    }
    output.sync_all().await?;
    drop(output);
    let temporary = tokio::task::spawn_blocking(move || -> Result<_> {
        verify_file(temporary.path())?;
        Ok(temporary)
    })
    .await??;
    temporary.persist_noclobber(destination)?;
    Ok(())
}

/// An offline export can migrate its input; callers must supply a private database copy.
pub fn create_from_database(database: &Path, destination: &Path) -> Result<Header> {
    use rusqlite::OptionalExtension;
    ensure!(database.exists(), "source database does not exist");
    ensure!(!destination.exists(), "backup file already exists");
    let reader = rusqlite::Connection::open_with_flags(
        database,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let writer = reader
        .query_row(
            "SELECT value FROM meta WHERE key='backup_restore_writer'",
            [],
            |r| r.get::<_, String>(0),
        )
        .optional()?
        .unwrap_or_else(|| "backup-reader".into());
    drop(reader);
    let store = Store::open(database, writer)?;
    // Opening an older copy migrates its schema; finish the normal upgrade recovery
    // before describing the current registry and projections in the archive header.
    store.validate_replication_backlog()?;
    store.apply_replication_repairs()?;
    store.replay_replication_graph()?;
    let directory = destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
    let header = store.write_backup(&mut temporary)?;
    temporary.as_file().sync_all()?;
    temporary.persist_noclobber(destination)?;
    Ok(header)
}

fn read_record(input: &mut impl BufRead) -> Result<Option<(Record, Vec<u8>)>> {
    use std::io::Read;
    let mut bytes = Vec::new();
    input
        .take(MAX_RECORD_BYTES + 1)
        .read_until(b'\n', &mut bytes)?;
    if bytes.is_empty() {
        return Ok(None);
    }
    ensure!(
        bytes.len() as u64 <= MAX_RECORD_BYTES && bytes.last() == Some(&b'\n'),
        "oversized or truncated backup record"
    );
    let record = serde_json::from_slice(&bytes).context("decode backup record")?;
    Ok(Some((record, bytes)))
}

fn check_header(header: &Header) -> Result<()> {
    ensure!(
        header.format == FORMAT && header.version == VERSION,
        "unsupported claim backup format/version"
    );
    ensure!(
        header.tables.contains_key("claim_sources"),
        "backup lacks the claim source digest"
    );
    Ok(())
}

/// Check framing and completeness without admitting anything.
pub fn verify_file(path: &Path) -> Result<Header> {
    let mut input = BufReader::new(fs::File::open(path)?);
    let (Record::Header(header), bytes) = read_record(&mut input)?.context("empty backup")? else {
        anyhow::bail!("backup must start with a header");
    };
    check_header(&header)?;
    let mut hash = Sha256::new();
    hash.update(bytes);
    let mut envelopes = 0;
    loop {
        let (record, bytes) = read_record(&mut input)?.context("backup has no end record")?;
        match record {
            Record::Envelope(_) => envelopes += 1,
            Record::Checkpoint(_) | Record::Signatures(_) => {}
            Record::End {
                envelopes: expected,
                sha256,
            } => {
                ensure!(
                    envelopes == expected && hex::encode(hash.finalize()) == sha256,
                    "backup checksum/count mismatch"
                );
                ensure!(
                    read_record(&mut input)?.is_none(),
                    "data follows the backup end record"
                );
                return Ok(header);
            }
            Record::Header(_) => anyhow::bail!("duplicate backup header"),
        }
        hash.update(bytes);
    }
}

/// Build privately at the current schema. Failed restores never publish a partial graph.
pub fn restore(source: &Path, destination: &Path) -> Result<RestoreReport> {
    let header = verify_file(source)?;
    let existing = if destination.exists() {
        let connection = rusqlite::Connection::open(destination)?;
        connection.busy_timeout(std::time::Duration::ZERO)?;
        connection.execute_batch("BEGIN EXCLUSIVE;")?;
        require_empty_database(&connection)?;
        // A WAL file may outlive the last connection. Changing journal mode takes the locks
        // that distinguish a closed database from a live WAL reader, unlike file existence.
        // Keep the exclusive connection lock through publication so no new writer can enter.
        connection.execute_batch("COMMIT; PRAGMA locking_mode=EXCLUSIVE;")?;
        connection
            .execute_batch("PRAGMA journal_mode=DELETE; BEGIN EXCLUSIVE;")
            .context("restore requires an offline database")?;
        // Recheck after the journal-mode transition released the first transaction.
        require_empty_database(&connection)?;
        Some(connection)
    } else {
        None
    };
    let directory = destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(directory)?;
    let stage = tempfile::tempdir_in(directory)?;
    let staged_path = stage.path().join("claims.sqlite3");
    let writer = format!("restored-{}", uuid::Uuid::now_v7().simple());
    let store = Store::open(&staged_path, &writer)?;
    store.prepare_backup_restore(&header, &writer)?;
    let mut input = BufReader::new(fs::File::open(source)?);
    let (Record::Header(restoring_header), bytes) =
        read_record(&mut input)?.context("empty backup")?
    else {
        anyhow::bail!("backup header changed during restore");
    };
    ensure!(
        serde_json::to_value(&restoring_header)? == serde_json::to_value(&header)?,
        "backup header changed during restore"
    );
    let mut restore_hash = Sha256::new();
    restore_hash.update(bytes);
    let mut checkpoint_cursor = None;
    let mut checkpoint_finished = false;
    let mut count = 0;
    let mut page = Vec::with_capacity(PAGE);
    let mut checkpoint = None;
    loop {
        let (record, bytes) = read_record(&mut input)?.context("backup ended during restore")?;
        match record {
            Record::Envelope(envelope) => {
                page.push(envelope);
                count += 1;
                if page.len() == PAGE {
                    store.receive_backup_page(&header, &page)?;
                    page.clear();
                }
            }
            Record::Signatures(signatures) => {
                store.receive_backup_signatures(&header, &signatures)?
            }
            Record::Checkpoint(page) => {
                ensure!(!checkpoint_finished, "duplicate checkpoint manifest");
                let manifest = checkpoint.get_or_insert_with(|| CheckpointManifest {
                    checkpoint: page.checkpoint.clone(),
                    cut_unix_ms: page.cut_unix_ms,
                    ..CheckpointManifest::default()
                });
                checkpoint_cursor = manifest.append(page)?;
                checkpoint_finished = checkpoint_cursor.is_none();
            }
            Record::End {
                envelopes: expected,
                sha256,
            } => {
                ensure!(
                    count == expected && hex::encode(restore_hash.finalize()) == sha256,
                    "backup changed during restore"
                );
                ensure!(
                    read_record(&mut input)?.is_none() && checkpoint_cursor.is_none(),
                    "incomplete backup or trailing data"
                );
                break;
            }
            Record::Header(_) => anyhow::bail!("duplicate backup header"),
        }
        restore_hash.update(bytes);
    }
    store.receive_backup_page(&header, &page)?;
    let report = store.finish_backup_restore(&header, &writer, count, checkpoint.as_ref())?;
    drop(store);
    // Store closes/checkpoints its writer before publication; never publish WAL-dependent data.
    let connection = rusqlite::Connection::open(&staged_path)?;
    connection.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
    drop(connection);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&staged_path, fs::Permissions::from_mode(0o600))?;
    }
    fs::File::open(&staged_path)?.sync_all()?;
    if existing.is_some() {
        fs::rename(&staged_path, destination)?;
    } else {
        // A concurrent restore cannot overwrite a database that appeared after the initial check.
        fs::hard_link(&staged_path, destination)?;
    }
    drop(existing);
    fs::File::open(directory)?.sync_all()?;
    Ok(report)
}

fn require_empty_database(connection: &rusqlite::Connection) -> Result<()> {
    let has_table: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='claims')",
        [],
        |r| r.get(0),
    )?;
    if has_table {
        let has_claims: bool =
            connection.query_row("SELECT EXISTS(SELECT 1 FROM claims)", [], |r| r.get(0))?;
        ensure!(
            !has_claims,
            "restore refuses a database that already holds claims"
        );
        let has_checkpoint_table: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='checkpoint_claims')",
            [], |r| r.get(0),
        )?;
        let has_tombstones = has_checkpoint_table
            && connection.query_row("SELECT EXISTS(SELECT 1 FROM checkpoint_claims)", [], |r| {
                r.get::<_, bool>(0)
            })?;
        ensure!(
            !has_tombstones,
            "restore refuses a database that already holds checkpointed claims"
        );
    } else {
        let has_tables: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table')",
            [],
            |r| r.get(0),
        )?;
        ensure!(!has_tables, "restore refuses a nonempty database");
    }
    // An older reader can hold unknown claims only in the wire log. That is still data.
    for table in ["replica_envelopes", "checkpoint_envelopes"] {
        let exists: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
            [table],
            |r| r.get(0),
        )?;
        if exists {
            let has_envelopes: bool = connection.query_row(
                &format!("SELECT EXISTS(SELECT 1 FROM {table})"),
                [],
                |r| r.get(0),
            )?;
            ensure!(
                !has_envelopes,
                "restore refuses a database that already holds envelopes"
            );
        }
    }
    Ok(())
}
