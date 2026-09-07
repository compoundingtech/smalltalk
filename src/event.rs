//! Declared event-stream ingress.
//!
//! Events are ordinary inbox files with additional provenance frontmatter. Publication has its
//! own bounded, agent-local receipt ring rather than writing the agent's immutable Sent ledger.

use std::fs::{self, File, OpenOptions};
use std::io::{Read as _, Seek as _, Write as _};
use std::os::fd::{AsRawFd as _, FromRawFd as _};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::DirBuilderExt as _;
use std::os::unix::fs::MetadataExt as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use anyhow::Context as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::identity::AgentSelector;
use crate::message;

const EVENT_VERSION: u32 = 1;
const MAX_STATE_BYTES: u64 = 1_048_576;
pub const RING_CAPACITY: usize = 128;
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StreamEntry {
    event_id: String,
    filename: String,
    key: Option<String>,
    rendered_sha256: String,
    #[serde(default)]
    supersede: bool,
    #[serde(default)]
    predecessor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StreamPending {
    event_id: String,
    filename: String,
    key: Option<String>,
    rendered_sha256: String,
    supersede: bool,
    #[serde(default)]
    predecessor: Option<StreamEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StreamRecord {
    version: u32,
    stream: String,
    recipient: String,
    pending: Option<StreamPending>,
    recent: Vec<StreamEntry>,
}

impl StreamRecord {
    fn fresh(stream: &str, recipient: &str) -> Self {
        Self {
            version: EVENT_VERSION,
            stream: stream.to_owned(),
            recipient: recipient.to_owned(),
            pending: None,
            recent: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EventReceipt {
    pub recipient: String,
    pub stream: String,
    pub event_id: String,
    pub filename: String,
    pub status: EventReceiptStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub superseded: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum EventReceiptStatus {
    Created,
    Deduplicated,
}

struct ResolvedStream {
    /// The agent key this publication is owned by and persisted under: the positional
    /// `<host>.<identity>` bus identity, which an address cutover never moves.
    recipient: String,
    /// How to reach that subject's own directories again — an exact selector, so a publication
    /// keeps working across an address cutover.
    selector: AgentSelector,
}

enum StreamAdmission {
    Declared,
    BuiltinResync,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StreamOwnerIncarnation {
    catalog_lock_device: u64,
    catalog_lock_inode: u64,
    supervisor_pid: u32,
    supervisor_start_time_ticks: u64,
}

impl StreamOwnerIncarnation {
    pub(crate) fn occurrence_token(self, sequence: u64) -> String {
        format!(
            "v1:{}:{}:{}:{}:{sequence}",
            self.catalog_lock_device,
            self.catalog_lock_inode,
            self.supervisor_pid,
            self.supervisor_start_time_ticks
        )
    }

    #[cfg(test)]
    pub(crate) fn for_test(
        catalog_lock_device: u64,
        catalog_lock_inode: u64,
        supervisor_pid: u32,
        supervisor_start_time_ticks: u64,
    ) -> Self {
        Self {
            catalog_lock_device,
            catalog_lock_inode,
            supervisor_pid,
            supervisor_start_time_ticks,
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StreamOwnerBinding {
    schema: String,
    logical_host: String,
    catalog_lock_device: u64,
    catalog_lock_inode: u64,
    supervisor_pid: u32,
    supervisor_start_time_ticks: u64,
}

impl StreamOwnerBinding {
    fn incarnation(&self) -> StreamOwnerIncarnation {
        StreamOwnerIncarnation {
            catalog_lock_device: self.catalog_lock_device,
            catalog_lock_inode: self.catalog_lock_inode,
            supervisor_pid: self.supervisor_pid,
            supervisor_start_time_ticks: self.supervisor_start_time_ticks,
        }
    }
}

pub(crate) fn publish_owner_binding(root: &Path, host: &str) -> anyhow::Result<()> {
    let lock = crate::catalog_lock::CatalogLock::shared(root)?;
    publish_owner_binding_under_lock(root, host, &lock)
}

/// Publish against a caller-held catalog read fence so related declaration reads and this
/// incarnation binding observe one catalog generation.
pub(crate) fn publish_owner_binding_under_lock(
    root: &Path,
    host: &str,
    lock: &crate::catalog_lock::CatalogLock,
) -> anyhow::Result<()> {
    let metadata = lock.control().metadata()?;
    let binding = StreamOwnerBinding {
        schema: "st2.stream-owner.v1".to_owned(),
        logical_host: host.to_owned(),
        catalog_lock_device: metadata.dev(),
        catalog_lock_inode: metadata.ino(),
        supervisor_pid: std::process::id(),
        supervisor_start_time_ticks: crate::exec_backend::process_start_time_ticks(
            std::process::id() as i32,
        )?,
    };
    let path = crate::park::SupervisorScope::current(root, host)?.stream_owner_binding_path();
    fs::create_dir_all(path.parent().context("owner binding has no parent")?)?;
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    fs::write(&temporary, serde_json::to_vec(&binding)?)?;
    File::open(&temporary)?.sync_all()?;
    fs::rename(&temporary, &path)?;
    File::open(path.parent().context("owner binding has no parent")?)?.sync_all()?;
    Ok(())
}

pub(crate) fn clear_owner_binding(root: &Path, host: &str) {
    let Ok(path) = crate::park::SupervisorScope::current(root, host)
        .map(|scope| scope.stream_owner_binding_path())
    else {
        return;
    };
    let Ok(bytes) = fs::read(&path) else { return };
    let Ok(binding) = serde_json::from_slice::<StreamOwnerBinding>(&bytes) else {
        return;
    };
    if binding.supervisor_pid == std::process::id()
        && crate::exec_backend::process_start_time_ticks(std::process::id() as i32).ok()
            == Some(binding.supervisor_start_time_ticks)
    {
        let _ = fs::remove_file(path);
    }
}

#[doc(hidden)]
pub fn publish_owner_binding_for_test(root: &Path, host: &str) -> anyhow::Result<()> {
    publish_owner_binding(root, host)
}

fn read_valid_owner_binding(
    root: &Path,
    host: &str,
    lock: &crate::CatalogLock,
) -> anyhow::Result<StreamOwnerBinding> {
    let path = crate::park::SupervisorScope::current(root, host)?.stream_owner_binding_path();
    let binding: StreamOwnerBinding = serde_json::from_slice(
        &fs::read(&path)
            .with_context(|| format!("no active local stream owner binding for host '{host}'"))?,
    )?;
    let metadata = lock.control().metadata()?;
    anyhow::ensure!(
        binding.schema == "st2.stream-owner.v1"
            && binding.logical_host == host
            && binding.catalog_lock_device == metadata.dev()
            && binding.catalog_lock_inode == metadata.ino(),
        "stream owner binding does not match this catalog lock domain"
    );
    anyhow::ensure!(
        crate::host_lock::process_alive(binding.supervisor_pid as i32)
            && crate::exec_backend::process_start_time_ticks(binding.supervisor_pid as i32).ok()
                == Some(binding.supervisor_start_time_ticks),
        "stream owner binding for host '{host}' is stale"
    );
    Ok(binding)
}

pub(crate) fn current_stream_owner_incarnation(
    root: &Path,
    host: &str,
) -> anyhow::Result<StreamOwnerIncarnation> {
    let lock = crate::CatalogLock::shared(root)?;
    Ok(read_valid_owner_binding(root, host, &lock)?.incarnation())
}

fn validate_owner_binding(
    root: &Path,
    host: &str,
    lock: &crate::CatalogLock,
) -> anyhow::Result<()> {
    read_valid_owner_binding(root, host, lock).map(|_| ())
}

/// A publication the catalog refuses, as distinct from a failure that may pass later.
///
/// Eligibility is resolved under the shared catalog-authoring lock, so a refusal follows from the
/// declaration rather than from timing. Retrying one at a cadence is pure cost: it re-resolves the
/// whole catalog and discards the answer. The two kinds differ in what could change the answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RefusalKind {
    /// The recipient is declared but not running. Its desired state returning to running admits
    /// the same publication, so the reservation is worth keeping.
    RecipientNotRunning,
    /// Nothing about the recipient makes this admissible later: it is ambiguous, owned by another
    /// host, or does not declare the stream.
    Permanent,
}

#[derive(Debug)]
pub(crate) struct StreamRefusal {
    kind: RefusalKind,
    message: String,
}

impl StreamRefusal {
    fn new(kind: RefusalKind, message: String) -> anyhow::Error {
        anyhow::Error::new(Self { kind, message })
    }
}

impl std::fmt::Display for StreamRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for StreamRefusal {}

/// Classify a publication error. A `None` is a failure whose answer may differ next time — a
/// catalog mid-edit, an unreadable owner binding, a lock or I/O error — and stays retryable.
pub(crate) fn refusal_kind(error: &anyhow::Error) -> Option<RefusalKind> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<StreamRefusal>())
        .map(|refusal| refusal.kind)
}

/// Resolve one publication's recipient.
///
/// Ordinary references go through [`crate::identity::resolve_address`] — the same fail-closed
/// bare-or-qualified algorithm every other reference uses, with dedupe by agent ID — rather than
/// a hand-rolled precedence filter. The local host is tried first, so a bare address still names
/// this host's subject when another host declares the same address; a reference that only a
/// foreign host answers still resolves, and then meets the owner refusal below with its own
/// diagnostic instead of an unhelpful absence.
fn resolve_stream(
    root: &Path,
    this_host: &str,
    recipient: &AgentSelector,
    stream: &str,
    admission: StreamAdmission,
) -> anyhow::Result<ResolvedStream> {
    let reference = match recipient {
        AgentSelector::Id(id) => id.as_str(),
        AgentSelector::Address(address) => address.as_str(),
    };
    let discovered = crate::discover_strict(root);
    anyhow::ensure!(
        discovered.errors.is_empty(),
        "catalog has errors; refusing event publication: {}",
        discovered
            .errors
            .iter()
            .map(|error| format!("{}: {}", error.path.display(), error.message))
            .collect::<Vec<_>>()
            .join("; ")
    );
    // Two books, as everywhere else a reference resolves: retirement releases the address, so a
    // retired subject must not make a live claimant ambiguous — but it still answers to its own
    // address when nothing routable does, which is what turns "no such agent" into the
    // `RecipientNotRunning` refusal a supervisor can act on.
    let book = crate::identity::address_book(&discovered.specs, this_host);
    let selected = match crate::identity::resolve_local_first(&book, recipient, this_host) {
        Err(crate::identity::ResolveError::Unknown { .. }) => {
            let every_subject = discovered
                .specs
                .iter()
                .map(|spec| crate::identity::AddressBookEntry::of(spec, this_host))
                .collect::<Vec<_>>();
            crate::identity::resolve_local_first(&every_subject, recipient, this_host)
                .map(|entry| (entry.id.clone(), entry.bus_identity.clone()))
        }
        other => other.map(|entry| (entry.id.clone(), entry.bus_identity.clone())),
    };
    let (agent_id, bus_identity) = selected.map_err(|error| match error {
        crate::identity::ResolveError::Unknown { .. } => anyhow::anyhow!(
            "no agent '{reference}' found in catalog {}",
            root.display()
        ),
        error @ crate::identity::ResolveError::Ambiguous { .. } => StreamRefusal::new(
            RefusalKind::Permanent,
            format!("agent recipient '{reference}' is ambiguous; {error}"),
        ),
    })?;
    let mut claimants = discovered
        .specs
        .iter()
        .filter(|spec| spec.bus_id(this_host) == bus_identity);
    let spec = claimants
        .next()
        .context("the resolved agent left the discovery it was resolved against")?;
    // Two declarations under one key answer nothing decidably: the resolved subject and the spec
    // this walk would publish against could be different files.
    if claimants.next().is_some() {
        return Err(StreamRefusal::new(
            RefusalKind::Permanent,
            format!(
                "agent recipient '{reference}' names more than one declaration of '{bus_identity}'"
            ),
        ));
    }
    let key = bus_identity;
    if spec.resolved_host(this_host) != this_host {
        return Err(StreamRefusal::new(
            RefusalKind::Permanent,
            format!(
                "agent '{}' is owned by host '{}'; event publication must run on that host",
                spec.bus_id(this_host),
                spec.resolved_host(this_host)
            ),
        ));
    }
    match admission {
        StreamAdmission::Declared => {
            if !spec.streams.iter().any(|declared| declared.name == stream) {
                return Err(StreamRefusal::new(
                    RefusalKind::Permanent,
                    format!(
                        "agent '{}' does not declare stream '{stream}'",
                        spec.bus_id(this_host)
                    ),
                ));
            }
        }
        StreamAdmission::BuiltinResync => anyhow::ensure!(
            stream == crate::resync::RESYNC_STREAM,
            "built-in resync admission requires the reserved resync stream"
        ),
    }
    if !spec.desired_state.is_running() {
        return Err(StreamRefusal::new(
            RefusalKind::RecipientNotRunning,
            format!(
                "agent '{}' is {}; refusing event while its eyes are closed",
                spec.bus_id(this_host),
                spec.desired_state.as_str()
            ),
        ));
    }
    Ok(ResolvedStream {
        // The exact selector, not the address just resolved: the subject's own directories must
        // stay reachable through a later address cutover.
        selector: AgentSelector::Id(agent_id),
        recipient: key,
    })
}

pub fn render_event(
    from: &str,
    subject: Option<&str>,
    stream: &str,
    event_id: &str,
    key: Option<&str>,
    body: &str,
) -> String {
    let mut rendered = String::from("---\n");
    rendered.push_str(&format!("from: {from}\n"));
    if let Some(subject) = subject {
        rendered.push_str(&format!("subject: {subject}\n"));
    }
    rendered.push_str(&format!("stream: {stream}\n"));
    rendered.push_str(&format!("event-id: {event_id}\n"));
    if let Some(key) = key {
        rendered.push_str(&format!("key: {key}\n"));
    }
    rendered.push_str("---\n");
    rendered.push_str(body);
    if !body.ends_with('\n') {
        rendered.push('\n');
    }
    rendered
}

#[allow(clippy::too_many_arguments)]
pub fn emit(
    root: &Path,
    this_host: &str,
    recipient: &str,
    stream: &str,
    event_id: &str,
    key: Option<&str>,
    subject: Option<&str>,
    body: &str,
    supersede: bool,
) -> anyhow::Result<EventReceipt> {
    emit_admitted(
        root,
        this_host,
        &AgentSelector::Address(recipient.to_owned()),
        stream,
        event_id,
        key,
        subject,
        body,
        supersede,
        StreamAdmission::Declared,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_builtin_resync(
    root: &Path,
    this_host: &str,
    recipient: &str,
    event_id: &str,
    key: Option<&str>,
    subject: Option<&str>,
    body: &str,
    supersede: bool,
) -> anyhow::Result<EventReceipt> {
    emit_admitted(
        root,
        this_host,
        &AgentSelector::Id(recipient.to_owned()),
        crate::resync::RESYNC_STREAM,
        event_id,
        key,
        subject,
        body,
        supersede,
        StreamAdmission::BuiltinResync,
    )
}

#[allow(clippy::too_many_arguments)]
fn emit_admitted(
    root: &Path,
    this_host: &str,
    recipient: &AgentSelector,
    stream: &str,
    event_id: &str,
    key: Option<&str>,
    subject: Option<&str>,
    body: &str,
    supersede: bool,
    admission: StreamAdmission,
) -> anyhow::Result<EventReceipt> {
    validate_component("stream", stream)?;
    validate_component("event id", event_id)?;
    if let Some(key) = key {
        validate_component("event key", key)?;
    }
    if let Some(subject) = subject {
        validate_header("event subject", subject, 1_000)?;
    }
    // Serialize the eligibility observation with self-authoring and desired-state changes. Once a
    // suspension edit owns this lock, no later emit can publish from a stale running observation.
    let catalog_lock = crate::catalog_lock::CatalogLock::shared(root)?;
    validate_owner_binding(root, this_host, &catalog_lock)?;
    let ResolvedStream {
        recipient: canonical_recipient,
        selector,
    } = resolve_stream(root, this_host, recipient, stream, admission)?;
    let from = format!("{canonical_recipient}/{stream}");
    let rendered = render_event(&from, subject, stream, event_id, key, body);
    message::with_resolved_state_dir(
        root,
        &selector,
        this_host,
        &["resources", "streams", stream],
        true,
        |state_dir| {
            let _lock = StreamLock::exclusive(state_dir)?;
            let record_path = state_dir.join("state.json");
            let mut record = read_record(&record_path)?
                .unwrap_or_else(|| StreamRecord::fresh(stream, &canonical_recipient));
            anyhow::ensure!(
                record.version == EVENT_VERSION
                    && record.stream == stream
                    && record.recipient == canonical_recipient,
                "stream state for '{}#{stream}' is not readable at version {EVENT_VERSION}",
                canonical_recipient
            );

            if record
                .pending
                .as_ref()
                .is_some_and(|pending| pending.event_id != event_id)
            {
                let pending = record
                    .pending
                    .take()
                    .expect("different pending event was just observed");
                let materialized = message::with_resolved_message_boxes(
                    root,
                    &selector,
                    this_host,
                    |inbox, archive| {
                        let inbox_bytes = read_message_entry(inbox, &pending.filename)?;
                        let archive_bytes = read_message_entry(archive, &pending.filename)?;
                        if let Some(bytes) = inbox_bytes.as_deref() {
                            validate_pending_message(stream, &pending, bytes)?;
                        }
                        if let Some(bytes) = archive_bytes.as_deref() {
                            validate_pending_message(stream, &pending, bytes)?;
                        }
                        if inbox_bytes.is_some() || archive_bytes.is_some() {
                            if let Some(predecessor) = pending.predecessor.as_ref() {
                                finish_predecessor(stream, predecessor, inbox, archive)?;
                            }
                            Ok(true)
                        } else {
                            Ok(false)
                        }
                    },
                )?;
                if materialized {
                    record.recent.insert(
                        0,
                        StreamEntry {
                            event_id: pending.event_id,
                            filename: pending.filename,
                            key: pending.key,
                            rendered_sha256: pending.rendered_sha256,
                            supersede: pending.supersede,
                            predecessor: pending
                                .predecessor
                                .as_ref()
                                .map(|entry| entry.filename.clone()),
                        },
                    );
                    record.recent.truncate(RING_CAPACITY);
                }
                write_record(&record_path, &record)?;
            }

            let rendered_sha256 = hex_digest(rendered.as_bytes());
            if let Some(entry) = record
                .recent
                .iter()
                .find(|entry| entry.event_id == event_id)
            {
                anyhow::ensure!(
                    entry.rendered_sha256 == rendered_sha256,
                    "event identity `{stream}#{event_id}` reused with different content"
                );
                anyhow::ensure!(
                    entry.supersede == supersede,
                    "event identity `{stream}#{event_id}` reused with different supersession intent"
                );
                return Ok(EventReceipt {
                    recipient: canonical_recipient.clone(),
                    stream: stream.to_owned(),
                    event_id: event_id.to_owned(),
                    filename: entry.filename.clone(),
                    status: EventReceiptStatus::Deduplicated,
                    superseded: entry.predecessor.clone(),
                });
            }

            let (filename, resumed, predecessor) = match record.pending.as_ref() {
                Some(pending) if pending.event_id == event_id => {
                    anyhow::ensure!(
                        pending.rendered_sha256 == rendered_sha256
                            && pending.key.as_deref() == key
                            && pending.supersede == supersede,
                        "event identity `{stream}#{event_id}` reused with different content"
                    );
                    (pending.filename.clone(), true, pending.predecessor.clone())
                }
                Some(pending) => anyhow::bail!(
                    "stream '{stream}' has an interrupted event '{}'; replay it before publishing another",
                    pending.event_id
                ),
                None => {
                    let predecessor = if supersede {
                        message::with_resolved_message_boxes(
                            root,
                            &selector,
                            this_host,
                            |inbox, archive| {
                                for entry in record.recent.iter().filter(|entry| {
                                    key.is_none_or(|key| entry.key.as_deref() == Some(key))
                                }) {
                                    if read_message_entry(inbox, &entry.filename)?.is_some()
                                        && read_message_entry(archive, &entry.filename)?.is_none()
                                    {
                                        return Ok(Some(entry.clone()));
                                    }
                                }
                                Ok(None)
                            },
                        )?
                    } else {
                        None
                    };
                    (message::new_filename(), false, predecessor)
                }
            };
            if !resumed {
                record.pending = Some(StreamPending {
                    event_id: event_id.to_owned(),
                    filename: filename.clone(),
                    key: key.map(str::to_owned),
                    rendered_sha256: rendered_sha256.clone(),
                    supersede,
                    predecessor: predecessor.clone(),
                });
                write_record(&record_path, &record)?;
                test_event_checkpoint(event_id, "pending")?;
            }

            let created = message::with_resolved_message_boxes(
                root,
                &selector,
                this_host,
                |inbox, archive| {
                    // Publish before compacting. If predecessor archival fails or the process
                    // crashes between these operations, both records remain unread; replaying the
                    // durable pending reservation completes compaction without risking a lost wake.
                    let (created, published_dir) = match open_message_entry(archive, &filename)? {
                        Some((_, bytes)) => {
                            validate_pending_message(
                                stream,
                                record.pending.as_ref().expect("pending reservation exists"),
                                &bytes,
                            )?;
                            if let Some((_, inbox_bytes)) = open_message_entry(inbox, &filename)? {
                                validate_pending_message(
                                    stream,
                                    record.pending.as_ref().expect("pending reservation exists"),
                                    &inbox_bytes,
                                )?;
                            }
                            (false, archive)
                        }
                        None => match read_message_entry(inbox, &filename)? {
                            Some(bytes) => {
                                validate_pending_message(
                                    stream,
                                    record.pending.as_ref().expect("pending reservation exists"),
                                    &bytes,
                                )?;
                                (false, inbox)
                            }
                            None => (
                                message::materialize_message_once(inbox, &filename, &rendered)?,
                                inbox,
                            ),
                        },
                    };
                    sync_message_entry(published_dir, &filename)?;
                    test_event_checkpoint(event_id, "durable")?;
                    test_event_checkpoint(event_id, "materialized")?;
                    if let Some(predecessor) = predecessor.as_ref() {
                        finish_predecessor(stream, predecessor, inbox, archive)?;
                    }
                    Ok(created)
                },
            )?;

            record.pending = None;
            record.recent.insert(
                0,
                StreamEntry {
                    event_id: event_id.to_owned(),
                    filename: filename.clone(),
                    key: key.map(str::to_owned),
                    rendered_sha256,
                    supersede,
                    predecessor: predecessor.as_ref().map(|entry| entry.filename.clone()),
                },
            );
            record.recent.truncate(RING_CAPACITY);
            write_record(&record_path, &record)?;

            Ok(EventReceipt {
                recipient: canonical_recipient.clone(),
                stream: stream.to_owned(),
                event_id: event_id.to_owned(),
                filename,
                status: if created {
                    EventReceiptStatus::Created
                } else {
                    EventReceiptStatus::Deduplicated
                },
                superseded: predecessor.map(|entry| entry.filename),
            })
        },
    )
}

fn read_message_entry(directory: &Path, filename: &str) -> anyhow::Result<Option<Vec<u8>>> {
    Ok(open_message_entry(directory, filename)?.map(|(_, bytes)| bytes))
}

fn open_message_entry(directory: &Path, filename: &str) -> anyhow::Result<Option<(File, Vec<u8>)>> {
    anyhow::ensure!(
        message::is_message_filename(filename),
        "invalid pending message filename {filename:?}"
    );
    let path = directory.join(filename);
    match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(&path)
    {
        Ok(mut file) => {
            let metadata = file.metadata()?;
            anyhow::ensure!(
                metadata.is_file(),
                "pending message entry {filename:?} is not a real regular file"
            );
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)?;
            Ok(Some((file, bytes)))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn validate_pending_message(
    stream: &str,
    pending: &StreamPending,
    bytes: &[u8],
) -> anyhow::Result<()> {
    anyhow::ensure!(
        hex_digest(bytes) == pending.rendered_sha256,
        "pending event '{}#{}' reserved file has different bytes",
        stream,
        pending.event_id
    );
    let contents = std::str::from_utf8(bytes).context("pending event record is not UTF-8")?;
    let parsed = message::parse_message(&pending.filename, contents);
    anyhow::ensure!(
        parsed.stream.as_deref() == Some(stream)
            && parsed.event_id.as_deref() == Some(pending.event_id.as_str())
            && parsed.event_key.as_deref() == pending.key.as_deref(),
        "pending event '{}#{}' reserved file has different event identity",
        stream,
        pending.event_id
    );
    Ok(())
}

fn sync_message_entry(directory: &Path, filename: &str) -> anyhow::Result<()> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(directory.join(filename))?;
    anyhow::ensure!(
        file.metadata()?.is_file(),
        "event publication is not a regular file"
    );
    file.sync_all()?;
    File::open(directory)?.sync_all()?;
    Ok(())
}

fn finish_predecessor(
    stream: &str,
    predecessor: &StreamEntry,
    inbox: &Path,
    archive: &Path,
) -> anyhow::Result<()> {
    let inbox_entry = open_message_entry(inbox, &predecessor.filename)?;
    let archive_entry = open_message_entry(archive, &predecessor.filename)?;
    if let Some((_, bytes)) = inbox_entry.as_ref() {
        validate_retained_message(stream, predecessor, bytes)?;
    }
    if let Some((_, bytes)) = archive_entry.as_ref() {
        validate_retained_message(stream, predecessor, bytes)?;
    }
    anyhow::ensure!(
        inbox_entry.is_some() || archive_entry.is_some(),
        "supersession predecessor '{}#{}' has no inbox file or archive receipt",
        stream,
        predecessor.event_id
    );
    if archive_entry.is_none()
        && let Some((file, _)) = inbox_entry.as_ref()
    {
        test_predecessor_archive_checkpoint()?;
        archive_validated_file(file, inbox, archive, &predecessor.filename)?;
    } else if let (Some((inbox_file, _)), Some((archive_file, _))) =
        (inbox_entry.as_ref(), archive_entry.as_ref())
    {
        conditional_unlink_same_inode(inbox_file, archive_file, inbox, &predecessor.filename)?;
    }
    Ok(())
}

#[cfg(debug_assertions)]
fn test_predecessor_archive_checkpoint() -> anyhow::Result<()> {
    let (Ok(ready), Ok(release)) = (
        std::env::var("ST2_TEST_EVENT_ARCHIVE_READY"),
        std::env::var("ST2_TEST_EVENT_ARCHIVE_RELEASE"),
    ) else {
        return Ok(());
    };
    fs::write(&ready, b"validated")?;
    while !Path::new(&release).exists() {
        std::thread::yield_now();
    }
    Ok(())
}

#[cfg(not(debug_assertions))]
fn test_predecessor_archive_checkpoint() -> anyhow::Result<()> {
    Ok(())
}

fn archive_validated_file(
    file: &File,
    inbox: &Path,
    archive: &Path,
    filename: &str,
) -> anyhow::Result<()> {
    use std::ffi::CString;

    fs::create_dir_all(archive)?;
    let archive_dir = File::open(archive)?;
    #[cfg(target_os = "linux")]
    let capability = format!("/proc/self/fd/{}", file.as_raw_fd());
    #[cfg(not(target_os = "linux"))]
    let capability = format!("/dev/fd/{}", file.as_raw_fd());
    let capability = CString::new(capability)?;
    let filename_c = CString::new(std::ffi::OsStr::new(filename).as_bytes())?;
    let result = unsafe {
        libc::linkat(
            libc::AT_FDCWD,
            capability.as_ptr(),
            archive_dir.as_raw_fd(),
            filename_c.as_ptr(),
            libc::AT_SYMLINK_FOLLOW,
        )
    };
    if result < 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            // A receipt for this predecessor already exists; the readback below proves it
            // carries exactly the validated bytes.
        } else if capability_link_unsupported(&error) {
            // The platform cannot hardlink through the open-file descriptor path at all;
            // degrade to a byte-copy receipt instead of failing publication.
            write_archive_receipt_copy(file, &archive_dir, archive, filename)?;
        } else {
            return Err(error).context("archive the validated predecessor capability");
        }
    }
    let archived = read_message_entry(archive, filename)?
        .context("validated predecessor archive receipt disappeared")?;
    let mut source = file.try_clone()?;
    source.rewind()?;
    let mut expected = Vec::new();
    source.read_to_end(&mut expected)?;
    anyhow::ensure!(archived == expected, "archive receipt has different bytes");
    File::open(archive.join(filename))?.sync_all()?;
    archive_dir.sync_all()?;
    conditional_unlink_same_inode(file, file, inbox, filename)?;
    File::open(inbox)?.sync_all()?;
    Ok(())
}

/// Whether linkat through the open-file capability path is unsupported by the platform rather
/// than a real failure. macOS fdescfs answers linkat(AT_SYMLINK_FOLLOW) on /dev/fd/N with
/// EPERM; ENOSYS/EOPNOTSUPP cover kernels lacking the syscall or its symlink-follow semantics.
/// Everything else stays a hard error so genuine failures (permissions, cross-device, ...)
/// surface instead of being silently degraded to a copy.
fn capability_link_unsupported(error: &std::io::Error) -> bool {
    #[cfg(debug_assertions)]
    if TEST_FORCE_ARCHIVE_RECEIPT_COPY.load(Ordering::Relaxed) {
        return true;
    }
    matches!(
        error.raw_os_error(),
        Some(libc::EPERM) | Some(libc::ENOSYS) | Some(libc::EOPNOTSUPP)
    )
}

/// Debug-only switch letting tests exercise the byte-copy fallback on platforms where the real
/// capability linkat would succeed. Not a supported configuration knob. Flipping this mid-run
/// is safe: the fallback receipt is verified against the validated bytes exactly like the
/// hardlink path, and the same-inode unlink treats a copy receipt as "archived" regardless.
#[cfg(debug_assertions)]
pub(crate) static TEST_FORCE_ARCHIVE_RECEIPT_COPY: AtomicBool = AtomicBool::new(false);

/// Materialize the archive receipt as a byte copy of the validated file, for platforms that
/// cannot hardlink through the open-file descriptor path.
///
/// The staged temp keeps a concurrent archiver from observing a partial receipt, and
/// rename_noreplace turns an install race into a no-op: whichever receipt wins, the caller's
/// readback proves it carries exactly the validated bytes. The tradeoff against the hardlink
/// fast path is inode identity -- a crash between the copy and the conditional unlink leaves
/// the retained inbox entry in place until revalidation, which the same-inode checks read as
/// "still present", never as data loss.
fn write_archive_receipt_copy(
    file: &File,
    archive_dir: &File,
    archive: &Path,
    filename: &str,
) -> anyhow::Result<()> {
    let mut source = file.try_clone()?;
    source.rewind()?;
    let mut bytes = Vec::new();
    source.read_to_end(&mut bytes)?;
    drop(source);

    let staged = archive.join(format!(
        ".st2-archive-{}-{}",
        std::process::id(),
        TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let mut staged_file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&staged)?;
    staged_file.write_all(&bytes)?;
    staged_file.sync_all()?;
    drop(staged_file);

    let target = archive.join(filename);
    if let Err(error) = crate::catalog_transaction::rename_noreplace(&staged, &target) {
        fs::remove_file(&staged).context("remove staging copy after failed receipt install")?;
        if error.kind() != std::io::ErrorKind::AlreadyExists {
            return Err(error).context("install archived predecessor receipt");
        }
    }
    archive_dir.sync_all()?;
    Ok(())
}

fn conditional_unlink_same_inode(
    inbox_file: &File,
    expected_file: &File,
    inbox: &Path,
    filename: &str,
) -> anyhow::Result<()> {
    let validated = expected_file.metadata()?;
    let opened = inbox_file.metadata()?;
    if opened.dev() != validated.dev() || opened.ino() != validated.ino() {
        return Ok(());
    }
    let quarantine = inbox.join(format!(
        ".st2-unlink-{}-{}",
        std::process::id(),
        TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    fs::DirBuilder::new().mode(0o700).create(&quarantine)?;
    let source = inbox.join(filename);
    let isolated = quarantine.join(filename);
    match fs::rename(&source, &isolated) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::remove_dir(&quarantine)?;
            return Ok(());
        }
        Err(error) => return Err(error).context("isolate predecessor before conditional unlink"),
    }

    let isolated_file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(&isolated)?;
    let isolated_metadata = isolated_file.metadata()?;
    if isolated_metadata.dev() == validated.dev() && isolated_metadata.ino() == validated.ino() {
        fs::remove_file(&isolated)?;
        fs::remove_dir(&quarantine)?;
        File::open(inbox)?.sync_all()?;
    } else {
        crate::catalog_transaction::rename_noreplace(&isolated, &source)
            .context("restore concurrently replaced predecessor after atomic isolation")?;
        fs::remove_dir(&quarantine)?;
    }
    Ok(())
}

fn validate_retained_message(
    stream: &str,
    entry: &StreamEntry,
    bytes: &[u8],
) -> anyhow::Result<()> {
    anyhow::ensure!(
        hex_digest(bytes) == entry.rendered_sha256,
        "supersession predecessor '{}#{}' has different bytes",
        stream,
        entry.event_id
    );
    let contents = std::str::from_utf8(bytes).context("supersession predecessor is not UTF-8")?;
    let parsed = message::parse_message(&entry.filename, contents);
    anyhow::ensure!(
        parsed.stream.as_deref() == Some(stream)
            && parsed.event_id.as_deref() == Some(entry.event_id.as_str())
            && parsed.event_key.as_deref() == entry.key.as_deref(),
        "supersession predecessor '{}#{}' has different event identity",
        stream,
        entry.event_id
    );
    Ok(())
}

#[cfg(debug_assertions)]
fn test_event_checkpoint(event_id: &str, point: &str) -> anyhow::Result<()> {
    if std::env::var("ST2_TEST_EVENT_FAIL_AT").as_deref()
        == Ok(format!("{event_id}:{point}").as_str())
    {
        anyhow::bail!("injected event failure at {point}");
    }
    Ok(())
}

#[cfg(not(debug_assertions))]
fn test_event_checkpoint(_event_id: &str, _point: &str) -> anyhow::Result<()> {
    Ok(())
}

fn validate_component(label: &str, value: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !value.is_empty()
            && value.len() <= 200
            && value.trim() == value
            && !value.chars().any(char::is_control),
        "{label} must be 1..=200 bytes without surrounding whitespace or controls"
    );
    Ok(())
}

fn validate_header(label: &str, value: &str, max_bytes: usize) -> anyhow::Result<()> {
    anyhow::ensure!(
        !value.is_empty()
            && value.len() <= max_bytes
            && value.trim() == value
            && !value.chars().any(char::is_control),
        "{label} must be 1..={max_bytes} bytes without surrounding whitespace or controls"
    );
    Ok(())
}

fn hex_digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn read_record(path: &Path) -> anyhow::Result<Option<StreamRecord>> {
    match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(mut file) => {
            let metadata = file.metadata()?;
            anyhow::ensure!(
                metadata.is_file(),
                "stream state {} is not a real regular file",
                path.display()
            );
            anyhow::ensure!(
                metadata.len() <= MAX_STATE_BYTES,
                "stream state {} exceeds {MAX_STATE_BYTES} bytes",
                path.display()
            );
            let mut bytes = Vec::with_capacity(metadata.len() as usize);
            file.read_to_end(&mut bytes)?;
            Ok(Some(serde_json::from_slice(&bytes).with_context(|| {
                format!("stream state {} is malformed", path.display())
            })?))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn write_record(path: &Path, record: &StreamRecord) -> anyhow::Result<()> {
    use std::ffi::CString;

    let parent = path.parent().context("stream state has no parent")?;
    fs::create_dir_all(parent)?;
    let directory = File::open(parent)
        .with_context(|| format!("open stream state directory {}", parent.display()))?;
    let temporary = format!(
        ".state.tmp-{}-{}",
        std::process::id(),
        TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let temporary = CString::new(temporary)?;
    let target = CString::new(
        path.file_name()
            .context("stream state has no filename")?
            .as_encoded_bytes(),
    )?;
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            temporary.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| {
            format!(
                "create fresh stream state temporary in {}",
                parent.display()
            )
        });
    }
    let mut file = unsafe { File::from_raw_fd(fd) };
    let result = (|| -> anyhow::Result<()> {
        file.write_all(&serde_json::to_vec(record)?)?;
        file.sync_all()?;
        let renamed = unsafe {
            libc::renameat(
                directory.as_raw_fd(),
                temporary.as_ptr(),
                directory.as_raw_fd(),
                target.as_ptr(),
            )
        };
        if renamed != 0 {
            return Err(std::io::Error::last_os_error()).context("publish stream state atomically");
        }
        directory.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        unsafe {
            libc::unlinkat(directory.as_raw_fd(), temporary.as_ptr(), 0);
        }
    }
    result?;
    Ok(())
}

struct StreamLock(File);

impl StreamLock {
    fn exclusive(state_dir: &Path) -> anyhow::Result<Self> {
        use std::os::fd::AsRawFd as _;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(state_dir.join(".lock"))?;
        anyhow::ensure!(
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0,
            "locking stream state failed"
        );
        Ok(Self(file))
    }
}

impl Drop for StreamLock {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd as _;
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn declare_worker(root: &Path, extra: &str) -> PathBuf {
        let directory = root.join("agents/hetz/worker");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join("agent.kdl"),
            format!(
                "agent \"worker\" {{\n  host \"hetz\"\n{extra}  desired-state \"running\"\n  command \"agent\"\n}}\n"
            ),
        )
        .unwrap();
        publish_owner_binding_for_test(root, "hetz").unwrap();
        directory
    }

    fn stream_state(agent: &Path, stream: &str) -> StreamRecord {
        let path = agent
            .join("resources/streams")
            .join(stream)
            .join("state.json");
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
    }

    /// `st2 event emit` names its recipient by ordinary bus address, so a declared `address` is
    /// what routes — bare or host-qualified — and the released identity spelling refuses at once.
    /// Ownership stays on the positional bus identity, which the cutover never moved.
    #[test]
    fn a_declared_address_routes_an_event_while_ownership_stays_on_the_bus_identity() {
        let root = tempfile::tempdir().unwrap();
        let agent = declare_worker(
            root.path(),
            "  address \"chat\"\n  stream \"gh-ci\" {}\n",
        );

        for reference in ["hetz.chat", "chat"] {
            let receipt = emit(
                root.path(),
                "hetz",
                reference,
                "gh-ci",
                &format!("run-{reference}"),
                None,
                None,
                "{}",
                false,
            )
            .expect("the declared address routes");
            assert_eq!(receipt.recipient, "hetz.worker");
        }
        assert_eq!(stream_state(&agent, "gh-ci").recipient, "hetz.worker");

        let refused = emit(
            root.path(),
            "hetz",
            "hetz.worker",
            "gh-ci",
            "run-legacy",
            None,
            None,
            "{}",
            false,
        )
        .expect_err("the identity spelling is not an address once one is declared");
        assert!(
            format!("{refused:#}").contains("no agent 'hetz.worker' found"),
            "{refused:#}"
        );
    }

    /// Built-in resync names its recipient by the agent key reconciliation holds — the positional
    /// bus identity — so an address cutover cannot disconnect a running seat from its own resync
    /// stream. This is the nondisruptive half of the cutover, and it is why the exact selector
    /// answers to the positional key as well as to an explicit `id`.
    #[test]
    fn resync_still_reaches_a_subject_whose_address_moved() {
        let root = tempfile::tempdir().unwrap();
        let agent = declare_worker(root.path(), "  address \"chat\"\n");

        let receipt = emit_builtin_resync(
            root.path(),
            "hetz",
            "hetz.worker",
            "resync-1",
            None,
            None,
            "{}",
            false,
        )
        .expect("the agent key reaches its own subject after a cutover");
        assert_eq!(receipt.recipient, "hetz.worker");
        assert_eq!(receipt.status, EventReceiptStatus::Created);
        assert_eq!(
            stream_state(&agent, crate::resync::RESYNC_STREAM).recipient,
            "hetz.worker"
        );
        let inbox = crate::message::list_inbox(&crate::message::inbox_dir(&agent)).unwrap();
        assert_eq!(inbox.len(), 1);
        assert_eq!(
            inbox[0].from.as_deref(),
            Some(format!("hetz.worker/{}", crate::resync::RESYNC_STREAM).as_str())
        );
    }
}
