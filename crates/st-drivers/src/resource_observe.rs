//! Private, scope-owned request/receipt channel for demand observation.
//!
//! A CLI writes one bounded request record; the resident Resource Profile supervisor alone writes
//! its receipt. Both directions fail toward missing evidence. Atomic rename prevents partial JSON
//! from becoming control input, while deliberately omitting fsync means power loss can require a
//! retry but can never manufacture success.

use std::fs::{self, OpenOptions};
use std::io::{Read as _, Write as _};
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant, SystemTime};

use anyhow::Context as _;
use notify::RecommendedWatcher;
use serde::{Deserialize, Serialize};

use crate::park::SupervisorScope;
use crate::resource_profile::{BindingId, RegistrationToken, RuntimeOwner, SnapshotDigest};

pub const OBSERVE_REQUEST_SCHEMA: &str = "st2.resource-observe-request.v1";
pub const OBSERVE_RECEIPT_SCHEMA: &str = "st2.resource-observe-receipt.v1";
pub const MAX_PENDING_OBSERVE_REQUESTS: usize = 256;
pub const MAX_OBSERVE_RECEIPTS: usize = 256;
const MAX_CONTROL_RECORD_BYTES: u64 = 64 * 1024;
const MAX_CONTROL_TEXT_BYTES: usize = 16 * 1024;
const MAX_REQUEST_ID_BYTES: usize = 128;
const SCOPE_MODE: u32 = 0o700;
const RECORD_MODE: u32 = 0o600;
static REQUEST_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObserveAdmissionBackpressure {
    limit: usize,
}

impl ObserveAdmissionBackpressure {
    pub fn limit(self) -> usize {
        self.limit
    }
}

impl std::fmt::Display for ObserveAdmissionBackpressure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "durable Resource observation backlog is full (limit {})",
            self.limit
        )
    }
}

impl std::error::Error for ObserveAdmissionBackpressure {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ObserveRequest {
    pub schema: String,
    pub request_id: String,
    pub recipient: String,
    pub binding: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_catalog_generation: Option<u64>,
    /// Optional client fence against the host's currently accepted snapshot.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_snapshot_digest: Option<SnapshotDigest>,
    pub requested_at: String,
}

impl ObserveRequest {
    pub fn new(
        recipient: String,
        binding: String,
        expected_catalog_generation: Option<u64>,
        expected_snapshot_digest: Option<SnapshotDigest>,
    ) -> anyhow::Result<Self> {
        validate_control_text("recipient", &recipient)?;
        validate_control_text("binding", &binding)?;
        let request_id = format!(
            "{}-{}-{:x}",
            crate::message::now_ms(),
            std::process::id(),
            REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        );
        Ok(Self {
            schema: OBSERVE_REQUEST_SCHEMA.to_owned(),
            request_id,
            recipient,
            binding,
            expected_catalog_generation,
            expected_snapshot_digest,
            requested_at: crate::exec_backend::rfc3339_utc(SystemTime::now())?,
        })
    }

    pub(crate) fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.schema == OBSERVE_REQUEST_SCHEMA,
            "unsupported observe request schema {:?}",
            self.schema
        );
        validate_request_id(&self.request_id)?;
        validate_control_text("recipient", &self.recipient)?;
        validate_control_text("binding", &self.binding)?;
        validate_control_text("requestedAt", &self.requested_at)
    }

    pub(crate) fn stable_key(&self) -> String {
        format!("{}\0{}", self.recipient, self.binding)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ObservationAuthority {
    pub owner: RuntimeOwner,
    pub binding_id: BindingId,
    pub registration: RegistrationToken,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ObserveReceiptStatus {
    Accepted,
    Backpressured,
    SettledUnchanged,
    SettledChanged,
    SettledFailed,
    AbsentBinding,
    StaleGeneration,
    ProviderUnavailable,
}

impl ObserveReceiptStatus {
    pub fn is_terminal(self) -> bool {
        !matches!(self, Self::Accepted | Self::Backpressured)
    }

    pub fn is_success(self) -> bool {
        matches!(self, Self::SettledUnchanged | Self::SettledChanged)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Backpressured => "backpressured",
            Self::SettledUnchanged => "settled-unchanged",
            Self::SettledChanged => "settled-changed",
            Self::SettledFailed => "settled-failed",
            Self::AbsentBinding => "absent-binding",
            Self::StaleGeneration => "stale-generation",
            Self::ProviderUnavailable => "provider-unavailable",
        }
    }

    pub fn wire_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Backpressured => "backpressured",
            Self::SettledUnchanged => "settledUnchanged",
            Self::SettledChanged => "settledChanged",
            Self::SettledFailed => "settledFailed",
            Self::AbsentBinding => "absentBinding",
            Self::StaleGeneration => "staleGeneration",
            Self::ProviderUnavailable => "providerUnavailable",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ObserveReceipt {
    pub schema: String,
    pub request_id: String,
    pub recipient: String,
    pub binding: String,
    pub status: ObserveReceiptStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authority: Option<ObservationAuthority>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub demand_watermark: Option<u64>,
    /// Present only for `SettledChanged`; computed by the host from accepted publication bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub digest: Option<SnapshotDigest>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<String>,
    pub updated_at: String,
}

impl ObserveReceipt {
    pub(crate) fn new(
        request: &ObserveRequest,
        status: ObserveReceiptStatus,
        authority: Option<ObservationAuthority>,
        demand_watermark: Option<u64>,
        digest: Option<SnapshotDigest>,
        diagnostic: Option<String>,
    ) -> anyhow::Result<Self> {
        let diagnostic = normalize_diagnostic(diagnostic);
        Ok(Self {
            schema: OBSERVE_RECEIPT_SCHEMA.to_owned(),
            request_id: request.request_id.clone(),
            recipient: request.recipient.clone(),
            binding: request.binding.clone(),
            status,
            authority,
            demand_watermark,
            digest,
            diagnostic,
            updated_at: crate::exec_backend::rfc3339_utc(SystemTime::now())?,
        })
    }

    fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.schema == OBSERVE_RECEIPT_SCHEMA,
            "unsupported observe receipt schema {:?}",
            self.schema
        );
        validate_request_id(&self.request_id)?;
        validate_control_text("recipient", &self.recipient)?;
        validate_control_text("binding", &self.binding)?;
        validate_control_text("updatedAt", &self.updated_at)?;
        if let Some(diagnostic) = self.diagnostic.as_deref() {
            validate_control_text("diagnostic", diagnostic)?;
        }
        let dispatched = matches!(
            self.status,
            ObserveReceiptStatus::Accepted
                | ObserveReceiptStatus::Backpressured
                | ObserveReceiptStatus::SettledUnchanged
                | ObserveReceiptStatus::SettledChanged
                | ObserveReceiptStatus::SettledFailed
        );
        match (&self.authority, self.demand_watermark) {
            (Some(_), Some(watermark)) if watermark > 0 => {}
            (None, None) if !dispatched => {}
            (None, None) => anyhow::bail!("receipt status requires authority and demand watermark"),
            _ => anyhow::bail!(
                "receipt authority and positive demand watermark must be present together"
            ),
        }
        match self.status {
            ObserveReceiptStatus::SettledChanged => {
                anyhow::ensure!(
                    self.digest.is_some(),
                    "settled-changed receipt requires the accepted publication digest"
                );
            }
            _ => anyhow::ensure!(
                self.digest.is_none(),
                "only a settled-changed receipt may carry a publication digest"
            ),
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct ObserveWait {
    pub receipt: Option<ObserveReceipt>,
    pub timed_out: bool,
}

pub struct ObserveClient {
    receipt_path: PathBuf,
    wake: Receiver<()>,
    _watcher: Option<RecommendedWatcher>,
}

impl ObserveClient {
    pub fn wait_for_terminal(self, bound: Duration) -> anyhow::Result<ObserveWait> {
        let deadline = Instant::now()
            .checked_add(bound)
            .context("observe wait bound is too large")?;
        let mut last = None;
        loop {
            if let Some(receipt) = read_receipt_path(&self.receipt_path)? {
                let terminal = receipt.status.is_terminal();
                last = Some(receipt);
                if terminal {
                    return Ok(ObserveWait {
                        receipt: last,
                        timed_out: false,
                    });
                }
            }
            let now = Instant::now();
            if now >= deadline {
                return finish_wait_at_timeout(&self.receipt_path, last);
            }
            let remaining = deadline.saturating_duration_since(now);
            if self._watcher.is_some() {
                match self.wake.recv_timeout(remaining) {
                    Ok(()) => {}
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        return finish_wait_at_timeout(&self.receipt_path, last);
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        std::thread::sleep(remaining.min(Duration::from_millis(25)));
                    }
                }
            } else {
                std::thread::sleep(remaining.min(Duration::from_millis(25)));
            }
        }
    }
}

pub fn catalog_generation(catalog_root: &Path) -> anyhow::Result<Option<u64>> {
    crate::catalog_lock::read_generation_token(catalog_root)
}

/// Prove a live resident supervisor, install the receipt watch, then atomically publish the request.
/// Installing the watch before the write and always reading once before waiting closes the startup
/// race without any correctness timer.
pub fn submit_request(
    catalog_root: &Path,
    host: &str,
    request: &ObserveRequest,
) -> anyhow::Result<ObserveClient> {
    request.validate()?;
    crate::event::current_stream_owner_incarnation(catalog_root, host)
        .context("no live Resource Profile supervisor")?;
    let scope = SupervisorScope::current(catalog_root, host)?;
    prepare_scope(&scope)?;
    let receipt_dir = scope.observe_receipt_dir();
    let request_dir = scope.observe_request_dir();
    let (wake_tx, wake) = mpsc::channel();
    let watcher = crate::watch::watch_recursive_mutations(&receipt_dir, wake_tx).ok();
    let receipt_path = receipt_path(&receipt_dir, &request.request_id)?;
    let request_path = request_path(&request_dir, &request.request_id)?;
    let _lock = lock_request_scope(&request_dir)?;
    prune_request_temp_files(&request_dir)?;
    if !request_path.exists() && durable_request_capacity_is_full(&request_dir)? {
        return Err(ObserveAdmissionBackpressure {
            limit: MAX_PENDING_OBSERVE_REQUESTS,
        }
        .into());
    }
    write_json_atomically_no_fsync(&request_path, request)?;
    Ok(ObserveClient {
        receipt_path,
        wake,
        _watcher: watcher,
    })
}

pub(crate) fn prepare_scope(scope: &SupervisorScope) -> anyhow::Result<()> {
    ensure_private_directory(scope.root())?;
    ensure_private_directory(&scope.observe_request_dir())?;
    ensure_private_directory(&scope.observe_receipt_dir())?;
    let request_dir = scope.observe_request_dir();
    let _lock = lock_request_scope(&request_dir)?;
    prune_request_temp_files(&request_dir)
}

#[derive(Debug)]
pub(crate) struct PendingRequestRecord {
    pub request: ObserveRequest,
    pub modified_at: SystemTime,
    pub path: PathBuf,
}

pub(crate) fn scan_requests(dir: &Path) -> (Vec<PendingRequestRecord>, Vec<String>) {
    let mut records = Vec::new();
    let mut errors = Vec::new();
    {
        let _lock = match lock_request_scope(dir) {
            Ok(lock) => lock,
            Err(error) => {
                errors.push(format!(
                    "locking observe requests {}: {error:#}",
                    dir.display()
                ));
                return (records, errors);
            }
        };
        if let Err(error) = prune_request_temp_files(dir) {
            errors.push(format!(
                "pruning observe request temps {}: {error:#}",
                dir.display()
            ));
            return (records, errors);
        }
    }
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return (records, errors),
        Err(error) => {
            errors.push(format!(
                "listing observe requests {}: {error}",
                dir.display()
            ));
            return (records, errors);
        }
    };
    for (entry, request_id) in entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let request_id = final_request_id(name.to_str()?)?.to_owned();
            Some((entry, request_id))
        })
        .take(MAX_PENDING_OBSERVE_REQUESTS)
    {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let path = entry.path();
        let parsed = (|| -> anyhow::Result<PendingRequestRecord> {
            let metadata = entry.metadata()?;
            anyhow::ensure!(metadata.is_file(), "request record is not a regular file");
            let bytes = read_bounded_regular(&path)?;
            let request: ObserveRequest = serde_json::from_slice(&bytes)?;
            request.validate()?;
            anyhow::ensure!(
                request.request_id == request_id,
                "request id does not match final basename"
            );
            Ok(PendingRequestRecord {
                request,
                modified_at: metadata.modified().unwrap_or(SystemTime::now()),
                path: path.clone(),
            })
        })();
        match parsed {
            Ok(record) => records.push(record),
            Err(error) => {
                errors.push(format!("invalid observe request {name:?}: {error:#}"));
                let _ = remove_request(&path);
            }
        }
    }
    records.sort_by(|left, right| left.request.request_id.cmp(&right.request.request_id));
    (records, errors)
}

pub(crate) fn remove_request(path: &Path) -> anyhow::Result<()> {
    let request_dir = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("{} has no request directory", path.display()))?;
    let _lock = lock_request_scope(request_dir)?;
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("remove {}", path.display())),
    }
}

pub(crate) fn write_receipt(dir: &Path, receipt: &ObserveReceipt) -> anyhow::Result<()> {
    receipt.validate()?;
    let path = receipt_path(dir, &receipt.request_id)?;
    write_json_atomically_no_fsync(&path, receipt)
}

pub fn read_receipt(dir: &Path, request_id: &str) -> anyhow::Result<Option<ObserveReceipt>> {
    read_receipt_path(&receipt_path(dir, request_id)?)
}

fn read_receipt_path(path: &Path) -> anyhow::Result<Option<ObserveReceipt>> {
    let bytes = match read_bounded_regular(path) {
        Ok(bytes) => bytes,
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    let receipt: ObserveReceipt = serde_json::from_slice(&bytes)
        .with_context(|| format!("decode observe receipt {}", path.display()))?;
    receipt.validate()?;
    Ok(Some(receipt))
}

pub(crate) fn prune_terminal_receipts(dir: &Path) -> Vec<String> {
    let mut terminal = Vec::new();
    let mut errors = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return errors;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if final_request_id(name).is_none() {
            continue;
        }
        let path = entry.path();
        match read_receipt_path(&path) {
            Ok(Some(receipt)) if receipt.status.is_terminal() => {
                let modified = entry
                    .metadata()
                    .and_then(|metadata| metadata.modified())
                    .unwrap_or(SystemTime::UNIX_EPOCH);
                terminal.push((modified, path));
            }
            Ok(_) => {}
            Err(error) => errors.push(format!("inspect observe receipt {name:?}: {error:#}")),
        }
    }
    terminal.sort_by_key(|(modified, _)| *modified);
    let remove_count = terminal.len().saturating_sub(MAX_OBSERVE_RECEIPTS);
    for (_, path) in terminal.into_iter().take(remove_count) {
        if let Err(error) = fs::remove_file(&path) {
            errors.push(format!("prune observe receipt {}: {error}", path.display()));
        }
    }
    errors
}

fn request_path(dir: &Path, request_id: &str) -> anyhow::Result<PathBuf> {
    validate_request_id(request_id)?;
    Ok(dir.join(format!("{request_id}.json")))
}

fn receipt_path(dir: &Path, request_id: &str) -> anyhow::Result<PathBuf> {
    validate_request_id(request_id)?;
    Ok(dir.join(format!("{request_id}.json")))
}

fn final_request_id(name: &str) -> Option<&str> {
    let request_id = name.strip_suffix(".json")?;
    validate_request_id(request_id).ok()?;
    Some(request_id)
}

fn validate_request_id(request_id: &str) -> anyhow::Result<()> {
    let mut components = Path::new(request_id).components();
    let plain = matches!(
        (components.next(), components.next()),
        (Some(Component::Normal(_)), None)
    );
    anyhow::ensure!(
        plain
            && !request_id.starts_with('.')
            && request_id.len() <= MAX_REQUEST_ID_BYTES
            && request_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')),
        "{request_id:?} is not a valid observe request id"
    );
    Ok(())
}

fn validate_control_text(field: &str, value: &str) -> anyhow::Result<()> {
    anyhow::ensure!(!value.is_empty(), "{field} must not be empty");
    anyhow::ensure!(
        value.len() <= MAX_CONTROL_TEXT_BYTES,
        "{field} exceeds {MAX_CONTROL_TEXT_BYTES} bytes"
    );
    anyhow::ensure!(
        !value.bytes().any(|byte| byte == 0),
        "{field} contains a NUL byte"
    );
    Ok(())
}

fn normalize_diagnostic(diagnostic: Option<String>) -> Option<String> {
    let mut diagnostic = diagnostic?;
    if diagnostic.is_empty() {
        return None;
    }
    if diagnostic.contains('\0') {
        diagnostic = diagnostic.replace('\0', "\u{fffd}");
    }
    if diagnostic.len() > MAX_CONTROL_TEXT_BYTES {
        let mut end = MAX_CONTROL_TEXT_BYTES;
        while !diagnostic.is_char_boundary(end) {
            end -= 1;
        }
        diagnostic.truncate(end);
    }
    (!diagnostic.is_empty()).then_some(diagnostic)
}

fn durable_request_capacity_is_full(dir: &Path) -> anyhow::Result<bool> {
    let entries = fs::read_dir(dir).with_context(|| format!("list {}", dir.display()))?;
    Ok(entries
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .and_then(final_request_id)
                .is_some()
        })
        .take(MAX_PENDING_OBSERVE_REQUESTS)
        .count()
        >= MAX_PENDING_OBSERVE_REQUESTS)
}

fn prune_request_temp_files(dir: &Path) -> anyhow::Result<()> {
    let entries = fs::read_dir(dir).with_context(|| format!("list {}", dir.display()))?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.starts_with(".observe-") {
            fs::remove_file(entry.path())
                .with_context(|| format!("remove stale observe request temp {name:?}"))?;
        }
    }
    Ok(())
}

fn lock_request_scope(request_dir: &Path) -> anyhow::Result<crate::flock::FileLock> {
    let scope_root = request_dir.parent().ok_or_else(|| {
        anyhow::anyhow!(
            "observe request directory {} has no scope root",
            request_dir.display()
        )
    })?;
    ensure_private_directory(scope_root)?;
    let lock_path = scope_root.join(".observe.lock");
    let lock = crate::flock::open(&lock_path, crate::flock::Open::Create)
        .with_context(|| format!("open observe scope lock {}", lock_path.display()))?;
    // The transport only sets the mode at creation, so a lock file an older build left `0644`
    // stays `0644` without this.
    lock.set_permissions(fs::Permissions::from_mode(RECORD_MODE))
        .with_context(|| format!("set private permissions on {}", lock_path.display()))?;
    crate::flock::FileLock::hold_blocking(lock, crate::flock::Mode::Exclusive)
        .with_context(|| format!("lock observe scope {}", scope_root.display()))
}

fn finish_wait_at_timeout(
    receipt_path: &Path,
    last: Option<ObserveReceipt>,
) -> anyhow::Result<ObserveWait> {
    test_observe_wait_timeout_checkpoint();
    let latest = read_receipt_path(receipt_path)?;
    let receipt = latest.or(last);
    let timed_out = !receipt
        .as_ref()
        .is_some_and(|receipt| receipt.status.is_terminal());
    Ok(ObserveWait { receipt, timed_out })
}

#[cfg(debug_assertions)]
fn test_observe_wait_timeout_checkpoint() {
    let (Ok(ready), Ok(release)) = (
        std::env::var("ST2_TEST_OBSERVE_WAIT_TIMEOUT_READY"),
        std::env::var("ST2_TEST_OBSERVE_WAIT_TIMEOUT_RELEASE"),
    ) else {
        return;
    };
    let _ = fs::write(ready, b"ready");
    while !Path::new(&release).is_file() {
        std::thread::yield_now();
    }
}

#[cfg(not(debug_assertions))]
fn test_observe_wait_timeout_checkpoint() {}

fn ensure_private_directory(path: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(path).with_context(|| format!("create {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(SCOPE_MODE))
        .with_context(|| format!("set private permissions on {}", path.display()))
}

fn write_json_atomically_no_fsync<T: Serialize>(path: &Path, value: &T) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("{} has no parent", path.display()))?;
    ensure_private_directory(parent)?;
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    anyhow::ensure!(
        bytes.len() as u64 <= MAX_CONTROL_RECORD_BYTES,
        "control record exceeds {MAX_CONTROL_RECORD_BYTES} bytes"
    );
    let mut temp = tempfile::Builder::new()
        .prefix(".observe-")
        .tempfile_in(parent)?;
    temp.as_file()
        .set_permissions(fs::Permissions::from_mode(RECORD_MODE))?;
    temp.write_all(&bytes)?;
    temp.flush()?;
    temp.persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("publish {}", path.display()))?;
    Ok(())
}

fn read_bounded_regular(path: &Path) -> anyhow::Result<Vec<u8>> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    anyhow::ensure!(
        metadata.is_file(),
        "{} is not a regular file",
        path.display()
    );
    anyhow::ensure!(
        metadata.len() <= MAX_CONTROL_RECORD_BYTES,
        "{} exceeds {MAX_CONTROL_RECORD_BYTES} bytes",
        path.display()
    );
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_CONTROL_RECORD_BYTES + 1)
        .read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() as u64 <= MAX_CONTROL_RECORD_BYTES,
        "{} grew beyond {MAX_CONTROL_RECORD_BYTES} bytes",
        path.display()
    );
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_records_are_strict_private_and_round_trip() {
        let temp = tempfile::tempdir().unwrap();
        let scope = SupervisorScope::in_state_root(temp.path(), temp.path(), "host").unwrap();
        prepare_scope(&scope).unwrap();
        let request =
            ObserveRequest::new("h.worker".into(), "queue".into(), Some(9), None).unwrap();
        let path = request_path(&scope.observe_request_dir(), &request.request_id).unwrap();
        write_json_atomically_no_fsync(&path, &request).unwrap();
        let (records, errors) = scan_requests(&scope.observe_request_dir());
        assert!(errors.is_empty());
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].request, request);
        assert_eq!(
            fs::metadata(scope.root()).unwrap().permissions().mode() & 0o777,
            SCOPE_MODE
        );
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            RECORD_MODE
        );
    }

    #[test]
    fn final_basename_filter_prunes_crash_temps_without_starving_requests() {
        let temp = tempfile::tempdir().unwrap();
        let request_dir = temp.path().join("requests");
        let request = ObserveRequest::new("h.worker".into(), "queue".into(), None, None).unwrap();
        let path = request_path(&request_dir, &request.request_id).unwrap();
        write_json_atomically_no_fsync(&path, &request).unwrap();
        for index in 0..MAX_PENDING_OBSERVE_REQUESTS {
            fs::write(
                request_dir.join(format!(".observe-crash-{index}")),
                b"incomplete",
            )
            .unwrap();
        }
        fs::write(request_dir.join("sibling"), b"not json").unwrap();

        let (records, errors) = scan_requests(&request_dir);

        assert!(errors.is_empty());
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].request, request);
        assert!(
            fs::read_dir(&request_dir)
                .unwrap()
                .flatten()
                .all(|entry| !entry.file_name().to_string_lossy().starts_with(".observe-"))
        );
        assert!(!durable_request_capacity_is_full(&request_dir).unwrap());
        for invalid in ["../escape", "a/b", ".hidden", "x.json", "with space"] {
            assert!(
                request_path(&request_dir, invalid).is_err(),
                "accepted {invalid:?}"
            );
        }
    }

    #[test]
    fn receipt_taxonomy_is_terminal_only_on_evidence() {
        assert!(!ObserveReceiptStatus::Accepted.is_terminal());
        assert!(!ObserveReceiptStatus::Backpressured.is_terminal());
        assert!(ObserveReceiptStatus::SettledUnchanged.is_terminal());
        assert!(ObserveReceiptStatus::SettledChanged.is_success());
        assert!(!ObserveReceiptStatus::SettledFailed.is_success());
        assert!(!ObserveReceiptStatus::ProviderUnavailable.is_success());
    }

    #[test]
    fn receipt_status_keeps_human_and_wire_spellings_distinct() {
        assert_eq!(
            ObserveReceiptStatus::SettledUnchanged.as_str(),
            "settled-unchanged"
        );
        assert_eq!(
            ObserveReceiptStatus::SettledUnchanged.wire_str(),
            "settledUnchanged"
        );
        assert_eq!(
            ObserveReceiptStatus::ProviderUnavailable.wire_str(),
            "providerUnavailable"
        );
    }
    #[test]
    fn receipt_evidence_shape_matches_atomic_results() {
        let request =
            ObserveRequest::new("h.worker".into(), "queue".into(), Some(9), None).unwrap();
        let authority = ObservationAuthority {
            owner: RuntimeOwner::new(
                crate::resource_profile::RuntimeIncarnation::new("incarnation").unwrap(),
                crate::resource_profile::OwnerClaim::new("claim").unwrap(),
            ),
            binding_id: BindingId::new("binding").unwrap(),
            registration: RegistrationToken::new("registration").unwrap(),
        };

        let changed_without_digest = ObserveReceipt::new(
            &request,
            ObserveReceiptStatus::SettledChanged,
            Some(authority.clone()),
            Some(1),
            None,
            None,
        )
        .unwrap();
        assert!(changed_without_digest.validate().is_err());

        let digest = SnapshotDigest::of(b"accepted publication");
        let unchanged_with_digest = ObserveReceipt::new(
            &request,
            ObserveReceiptStatus::SettledUnchanged,
            Some(authority.clone()),
            Some(1),
            Some(digest),
            None,
        )
        .unwrap();
        assert!(unchanged_with_digest.validate().is_err());
        let predispatch_stale = ObserveReceipt::new(
            &request,
            ObserveReceiptStatus::StaleGeneration,
            None,
            None,
            None,
            Some("catalog changed".into()),
        )
        .unwrap();
        predispatch_stale.validate().unwrap();

        let active_stale = ObserveReceipt::new(
            &request,
            ObserveReceiptStatus::StaleGeneration,
            Some(authority.clone()),
            Some(1),
            None,
            Some("registration changed".into()),
        )
        .unwrap();
        active_stale.validate().unwrap();

        let partial_authority = ObserveReceipt::new(
            &request,
            ObserveReceiptStatus::ProviderUnavailable,
            Some(authority.clone()),
            None,
            None,
            Some("runtime exited".into()),
        )
        .unwrap();
        assert!(partial_authority.validate().is_err());

        let changed = ObserveReceipt::new(
            &request,
            ObserveReceiptStatus::SettledChanged,
            Some(authority),
            Some(1),
            Some(digest),
            None,
        )
        .unwrap();
        changed.validate().unwrap();
    }

    #[test]
    fn receipt_diagnostics_are_normalized_to_safe_optional_text() {
        let request =
            ObserveRequest::new("h.worker".into(), "queue".into(), Some(9), None).unwrap();
        let empty = ObserveReceipt::new(
            &request,
            ObserveReceiptStatus::ProviderUnavailable,
            None,
            None,
            None,
            Some(String::new()),
        )
        .unwrap();
        assert_eq!(empty.diagnostic, None);

        let with_nul = ObserveReceipt::new(
            &request,
            ObserveReceiptStatus::ProviderUnavailable,
            None,
            None,
            None,
            Some("provider\0refused".to_owned()),
        )
        .unwrap();
        assert_eq!(with_nul.diagnostic.as_deref(), Some("provider�refused"));
        with_nul.validate().unwrap();
    }

    #[test]
    fn request_scan_is_bounded_to_the_durable_admission_limit() {
        let temp = tempfile::tempdir().unwrap();
        let request_dir = temp.path().join("requests");
        for index in 0..(MAX_PENDING_OBSERVE_REQUESTS + 44) {
            let mut request =
                ObserveRequest::new("h.worker".into(), "queue".into(), Some(9), None).unwrap();
            request.request_id = format!("bounded-{index:03}");
            let path = request_path(&request_dir, &request.request_id).unwrap();
            write_json_atomically_no_fsync(&path, &request).unwrap();
        }

        let (records, errors) = scan_requests(&request_dir);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(records.len(), MAX_PENDING_OBSERVE_REQUESTS);
    }

    #[test]
    fn unknown_control_fields_are_rejected() {
        let raw = br#"{"schema":"st2.resource-observe-request.v1","requestId":"one","recipient":"h.a","binding":"x","requestedAt":"now","extra":true}"#;
        assert!(serde_json::from_slice::<ObserveRequest>(raw).is_err());
    }

    /// The transport hardened this lock file with `O_NOFOLLOW`; this module had no lock test
    /// before, and the scope lock is what serializes durable observe-request admission.
    #[test]
    fn a_symlinked_scope_lock_is_refused_instead_of_locking_its_target() {
        let tmp = tempfile::tempdir().unwrap();
        let scope_root = tmp.path().join("scope");
        let request_dir = scope_root.join("requests");
        fs::create_dir_all(&request_dir).unwrap();
        let outside = tmp.path().join("outside");
        fs::write(&outside, "unchanged").unwrap();
        std::os::unix::fs::symlink(&outside, scope_root.join(".observe.lock")).unwrap();

        let error = lock_request_scope(&request_dir).unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<std::io::Error>()
                .and_then(std::io::Error::raw_os_error),
            Some(libc::ELOOP),
            "a symlinked scope lock must be refused, got {error:#}"
        );
        assert_eq!(fs::read_to_string(&outside).unwrap(), "unchanged");
    }

    #[test]
    fn the_scope_lock_excludes_a_second_holder_and_releases_on_drop() {
        let tmp = tempfile::tempdir().unwrap();
        let request_dir = tmp.path().join("scope/requests");
        fs::create_dir_all(&request_dir).unwrap();
        let lock_path = tmp.path().join("scope/.observe.lock");
        let held = lock_request_scope(&request_dir).unwrap();
        assert_eq!(
            fs::metadata(&lock_path).unwrap().permissions().mode() & 0o777,
            RECORD_MODE
        );
        // `flock` locks the open file description, so a second open in this process contends.
        let probe = || {
            let file = crate::flock::open(&lock_path, crate::flock::Open::Existing).unwrap();
            crate::flock::FileLock::hold(
                file,
                crate::flock::Mode::Exclusive,
                crate::flock::Wait::Now,
            )
            .unwrap()
            .is_some()
        };
        assert!(
            !probe(),
            "a live scope-lock holder must exclude a second writer"
        );
        drop(held);
        assert!(probe(), "dropping the guard must release the scope lock");
    }
}
