//! The parked set's on-disk projection, and the operator's unpark request channel.
//!
//! [`FlappingCap`](crate::flapping::FlappingCap) is the source of truth for which tasks are parked,
//! and it lives in the supervisor's memory. Everything else — `st2 tasks` above all — is a separate,
//! read-only process, which is why a parked task used to show `desired: running`, not running, and
//! `error: null`: the fault existed, but only inside a process nobody was reading. This module is how
//! that in-memory truth becomes legible without becoming a second source of it.
//!
//! Two directions, one writer each. Conflating them is the tempting mistake:
//!
//! * The **projection** (`parked/`) is written by the supervisor and read by observers in its scope.
//!   A missing or absent marker means *not parked*. Nothing else may write here.
//! * The **request** (`unpark/`) is written by `st2 unpark` and consumed by the supervisor. A missing
//!   request means *nobody asked*.
//!
//! Were the projection also the request — "no marker, therefore unpark" — then a wiped state dir, a
//! failed write, or a supervisor and CLI disagreeing about `XDG_STATE_HOME` would all silently mean
//! *release every parked task in that supervisor scope*. Each direction instead fails towards
//! inaction. Both directions use the same scope: the canonical catalog folder plus host. Hashing that
//! tuple keeps same-host supervisors from deleting each other's markers or consuming each other's
//! requests, including when their task IDs collide.
//!
//! A marker is stamped with the generation of the supervisor that wrote it, and a marker whose
//! supervisor is gone reads as **not parked**. That is not leniency: R31 scopes parking to one
//! supervisor run, so no run means no park. It also keeps `st2 tasks` quiet and zero-exit for a scope
//! whose supervisor is not running. The generation is `(pid, start-time)` rather than a bare pid,
//! because a recycled pid would otherwise resurrect a park from a supervisor that died days ago.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

/// Wire schema of one park marker.
pub const PARK_SCHEMA: &str = "st2.park.v1";

fn hash_scope_component(hash: &mut Sha256, bytes: &[u8]) {
    hash.update((bytes.len() as u64).to_be_bytes());
    hash.update(bytes);
}

/// The canonical machine-local ownership scope of one supervisor. The directory name is a stable
/// SHA-256 of the length-framed canonical catalog path and host; there is no host-only fallback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupervisorScope {
    root: PathBuf,
}

impl SupervisorScope {
    pub fn current(catalog_root: &Path, host: &str) -> anyhow::Result<Self> {
        Self::in_state_root(&crate::run::state_root(), catalog_root, host)
    }

    pub(crate) fn in_state_root(
        state_root: &Path,
        catalog_root: &Path,
        host: &str,
    ) -> anyhow::Result<Self> {
        let catalog_root = catalog_root.canonicalize().with_context(|| {
            format!("canonicalize supervisor catalog {}", catalog_root.display())
        })?;
        let mut hash = Sha256::new();
        hash.update(b"st2.supervisor-scope.v1");
        hash_scope_component(&mut hash, catalog_root.as_os_str().as_bytes());
        hash_scope_component(&mut hash, host.as_bytes());
        let scope_id = format!("sha256-{:x}", hash.finalize());
        Ok(Self {
            root: state_root.join("st2/supervisors").join(scope_id),
        })
    }
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub fn park_dir(&self) -> PathBuf {
        self.root.join("parked")
    }

    pub fn unpark_request_dir(&self) -> PathBuf {
        self.root.join("unpark")
    }
    pub(crate) fn observe_request_dir(&self) -> PathBuf {
        self.root.join("observe-requests")
    }

    pub(crate) fn observe_receipt_dir(&self) -> PathBuf {
        self.root.join("observe-receipts")
    }

    pub(crate) fn stream_owner_binding_path(&self) -> PathBuf {
        self.root.join("stream-owner.json")
    }
}

/// One parked task, as published by the supervisor that parked it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParkRecord {
    pub schema: String,
    pub runtime_id: String,
    /// The supervisor that parked it, identified by pid *and* start time. Both must match a live
    /// process for the marker to count — a pid alone is reuse-unsafe.
    pub supervisor_pid: u32,
    pub supervisor_start_time_ticks: u64,
    /// When the park was published, RFC3339 UTC with milliseconds.
    pub parked_at: String,
    /// Why, in the operator's terms.
    pub reason: String,
}

/// What the projection says about one desired task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParkState {
    /// Positively not parked: no marker, or a marker owned by a supervisor that is gone.
    NotParked,
    Parked(ParkRecord),
    /// The marker exists but could not be believed. Unreadable evidence is not absence.
    Indeterminate(String),
}

/// One coherent read of the projection. `complete` false means some marker could not be believed, not
/// that some task is parked — a parked task is a *known* fault and keeps the batch complete.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParkBatch {
    pub complete: bool,
    pub states: BTreeMap<String, ParkState>,
    pub errors: Vec<String>,
}

impl ParkBatch {
    pub fn state(&self, runtime_id: &str) -> &ParkState {
        self.states.get(runtime_id).unwrap_or(&ParkState::NotParked)
    }
}

/// Read-only boundary over the projection, so the inventory can be driven deterministically in tests
/// without a real supervisor or a real state dir.
pub trait ParkObserver {
    fn observe(&self, desired: &[String]) -> ParkBatch;
}

/// The real projection reader, rooted at one supervisor scope's park dir.
#[derive(Debug, Clone)]
pub struct DirParkObserver {
    dir: PathBuf,
}

#[derive(Debug, PartialEq, Eq)]
enum ProcessStartTimeObservation {
    Running(u64),
    Gone,
    Indeterminate(String),
}

fn observe_process_start_time(pid: i32) -> ProcessStartTimeObservation {
    match crate::exec_backend::process_start_time_ticks(pid) {
        Ok(start_time_ticks) => ProcessStartTimeObservation::Running(start_time_ticks),
        Err(error) if crate::host_lock::process_alive(pid) => {
            ProcessStartTimeObservation::Indeterminate(error.to_string())
        }
        Err(_) => ProcessStartTimeObservation::Gone,
    }
}

impl DirParkObserver {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub fn for_supervisor(catalog_root: &Path, host: &str) -> anyhow::Result<Self> {
        Ok(Self::new(
            SupervisorScope::current(catalog_root, host)?.park_dir(),
        ))
    }

    fn observe_with(
        &self,
        desired: &[String],
        observe_start_time: &dyn Fn(i32) -> ProcessStartTimeObservation,
    ) -> ParkBatch {
        let mut batch = ParkBatch {
            complete: true,
            ..Default::default()
        };
        for runtime_id in desired {
            let state = match read_marker(&self.dir, runtime_id, observe_start_time) {
                Ok(state) => state,
                Err(error) => {
                    batch.complete = false;
                    let detail = format!("park marker for {runtime_id:?}: {error}");
                    batch.errors.push(detail.clone());
                    ParkState::Indeterminate(detail)
                }
            };
            batch.states.insert(runtime_id.clone(), state);
        }
        batch
    }
}

impl ParkObserver for DirParkObserver {
    fn observe(&self, desired: &[String]) -> ParkBatch {
        self.observe_with(desired, &observe_process_start_time)
    }
}

/// A projection with nothing in it — the honest state of a host whose supervisor has parked nothing.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoParks;

impl ParkObserver for NoParks {
    fn observe(&self, desired: &[String]) -> ParkBatch {
        ParkBatch {
            complete: true,
            states: desired
                .iter()
                .map(|id| (id.clone(), ParkState::NotParked))
                .collect(),
            errors: Vec::new(),
        }
    }
}

fn marker_path(dir: &Path, runtime_id: &str) -> PathBuf {
    dir.join(format!("{runtime_id}.json"))
}

fn read_marker(
    dir: &Path,
    runtime_id: &str,
    observe_start_time: &dyn Fn(i32) -> ProcessStartTimeObservation,
) -> anyhow::Result<ParkState> {
    let path = marker_path(dir, runtime_id);
    let raw = match fs::read(&path) {
        Ok(raw) => raw,
        // A missing marker — or a missing dir entirely — is positively "not parked". A host whose
        // supervisor never parked anything has no projection dir at all, and must not be an error.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ParkState::NotParked);
        }
        Err(error) => anyhow::bail!("reading {}: {error}", path.display()),
    };
    let record: ParkRecord =
        serde_json::from_slice(&raw).with_context(|| format!("parsing {}", path.display()))?;
    if record.schema != PARK_SCHEMA {
        anyhow::bail!(
            "unknown schema {:?} in {} (expected {PARK_SCHEMA})",
            record.schema,
            path.display()
        );
    }
    if record.runtime_id != runtime_id {
        anyhow::bail!(
            "{} claims runtime id {:?}",
            path.display(),
            record.runtime_id
        );
    }
    let supervisor_pid = i32::try_from(record.supervisor_pid)
        .context("park marker supervisor pid exceeds the process id range")?;
    if supervisor_pid <= 0 {
        anyhow::bail!("park marker supervisor pid must be positive");
    }
    if record.supervisor_start_time_ticks == 0 {
        anyhow::bail!("park marker supervisor start time must be positive");
    }
    match observe_start_time(supervisor_pid) {
        ProcessStartTimeObservation::Running(start_time_ticks)
            if start_time_ticks == record.supervisor_start_time_ticks => {}
        ProcessStartTimeObservation::Running(_) | ProcessStartTimeObservation::Gone => {
            // The supervisor that parked it is gone. R31 scopes a park to one supervisor run, so
            // this is a positive "not parked", not an unknown — and it keeps `st2 tasks` zero-exit
            // on a host where st2 is not running at all.
            return Ok(ParkState::NotParked);
        }
        ProcessStartTimeObservation::Indeterminate(error) => {
            anyhow::bail!("observing supervisor process generation: {error}");
        }
    }
    Ok(ParkState::Parked(record))
}

/// The supervisor's writer for the projection. Owns the dir exclusively.
#[derive(Debug, Clone)]
pub struct ParkProjection {
    dir: PathBuf,
    supervisor_pid: u32,
    supervisor_start_time_ticks: u64,
}

impl ParkProjection {
    /// Stamp the projection with *this* process's generation. Fails only if the supervisor cannot
    /// identify itself, which would make every marker it wrote unbelievable anyway.
    pub fn current(dir: PathBuf) -> anyhow::Result<Self> {
        let supervisor_pid = std::process::id();
        let supervisor_start_time_ticks =
            crate::exec_backend::process_start_time_ticks(supervisor_pid as i32)
                .context("identifying this supervisor's process generation")?;
        Ok(Self {
            dir,
            supervisor_pid,
            supervisor_start_time_ticks,
        })
    }

    pub fn for_supervisor(catalog_root: &Path, host: &str) -> anyhow::Result<Self> {
        Self::current(SupervisorScope::current(catalog_root, host)?.park_dir())
    }

    /// Republish the projection to exactly `parked`, returning any non-fatal errors. Markers for ids
    /// that are no longer parked are removed, so a recovered task stops reporting a fault without
    /// anyone having to remember to clean up after it.
    pub fn publish(&self, parked: &BTreeSet<String>, reason: &str) -> Vec<String> {
        let mut errors = Vec::new();
        if parked.is_empty() && !self.dir.exists() {
            return errors;
        }
        if let Err(error) = fs::create_dir_all(&self.dir) {
            errors.push(format!(
                "creating park projection {}: {error}",
                self.dir.display()
            ));
            return errors;
        }
        for runtime_id in parked {
            if let Err(error) = self.write_marker(runtime_id, reason) {
                errors.push(format!("publishing park for {runtime_id:?}: {error}"));
            }
        }
        match fs::read_dir(&self.dir) {
            Ok(entries) => {
                for entry in entries.flatten() {
                    let name = entry.file_name();
                    let Some(name) = name.to_str() else { continue };
                    let Some(runtime_id) = name.strip_suffix(".json") else {
                        continue;
                    };
                    if parked.contains(runtime_id) {
                        continue;
                    }
                    if let Err(error) = fs::remove_file(entry.path()) {
                        errors.push(format!("clearing stale park {runtime_id:?}: {error}"));
                    }
                }
            }
            Err(error) => errors.push(format!(
                "listing park projection {}: {error}",
                self.dir.display()
            )),
        }
        errors
    }

    fn write_marker(&self, runtime_id: &str, reason: &str) -> anyhow::Result<()> {
        let path = marker_path(&self.dir, runtime_id);
        // An already-published park keeps its original `parkedAt`: rewriting it every pass would make
        // "since when" read as "since a moment ago" forever, which is the field an operator uses to
        // tell a fresh crash-loop from one that has been down all day.
        if let Ok(raw) = fs::read(&path)
            && let Ok(existing) = serde_json::from_slice::<ParkRecord>(&raw)
            && existing.runtime_id == runtime_id
            && existing.supervisor_pid == self.supervisor_pid
            && existing.supervisor_start_time_ticks == self.supervisor_start_time_ticks
        {
            return Ok(());
        }
        let record = ParkRecord {
            schema: PARK_SCHEMA.to_string(),
            runtime_id: runtime_id.to_string(),
            supervisor_pid: self.supervisor_pid,
            supervisor_start_time_ticks: self.supervisor_start_time_ticks,
            parked_at: crate::exec_backend::rfc3339_utc(std::time::SystemTime::now())
                .context("timestamping park")?,
            reason: reason.to_string(),
        };
        write_json_atomically(&path, &record, ".park")
    }
}

/// The strictest publication level in the state plane, and deliberately so: a park marker is what
/// tells the next supervisor generation that a runtime is deliberately down, so a marker lost to a
/// crash reads as "nobody parked this" and the runtime comes back up.
fn write_json_atomically<T: Serialize>(
    path: &Path,
    value: &T,
    temp_prefix: &str,
) -> anyhow::Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    crate::fsatomic::replace(
        path,
        &bytes,
        crate::fsatomic::Staging::new(temp_prefix),
        crate::fsatomic::Durability::FsyncFileAndDir,
    )
    .with_context(|| format!("publishing {}", path.display()))
}

/// Reject anything that is not a plain filename, so an operator's typo (or a hostile argument) cannot
/// steer a request write outside the request dir. The projection side needs no equivalent: those ids
/// come from validated declarations, never from a command line.
fn validate_request_id(runtime_id: &str) -> anyhow::Result<()> {
    let mut components = Path::new(runtime_id).components();
    let single = matches!(
        (components.next(), components.next()),
        (Some(std::path::Component::Normal(_)), None)
    );
    anyhow::ensure!(
        single && !runtime_id.is_empty() && !runtime_id.starts_with('.'),
        "{runtime_id:?} is not a valid task id"
    );
    Ok(())
}

/// Ask the supervisor to unpark one task. Returns the request path. Writing a request that already
/// exists is not an error — the operator asked twice for the same thing.
pub fn request_unpark(dir: &Path, runtime_id: &str) -> anyhow::Result<PathBuf> {
    validate_request_id(runtime_id)?;
    let path = dir.join(runtime_id);
    fs::create_dir_all(dir)?;
    fs::File::create(&path).with_context(|| format!("writing {}", path.display()))?;
    fs::File::open(dir)?.sync_all()?;
    Ok(path)
}

/// Consume every pending unpark request, returning the requested ids and any non-fatal errors. Each
/// request is removed as it is taken, so one request produces exactly one recovery attempt: leaving it
/// behind would relaunch the task on every subsequent pass, defeating the restart policy entirely.
pub fn take_unpark_requests(dir: &Path) -> (Vec<String>, Vec<String>) {
    let mut ids = Vec::new();
    let mut errors = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return (ids, errors),
        Err(error) => {
            errors.push(format!(
                "listing unpark requests {}: {error}",
                dir.display()
            ));
            return (ids, errors);
        }
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.starts_with('.') {
            continue;
        }
        match fs::remove_file(entry.path()) {
            Ok(()) => ids.push(name.to_string()),
            // Another pass took it first. One request, one attempt — do not act on it twice.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => errors.push(format!("taking unpark request {name:?}: {error}")),
        }
    }
    ids.sort();
    (ids, errors)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// [`write_json_atomically`] is the strict end of the publication range and the reference the
    /// shared primitive is unified onto: the record's bytes are fsynced before the rename and the
    /// parent directory entry after it, and a directory that cannot be opened for that sync
    /// FAILS the publication rather than reporting success for a write that may not survive a
    /// crash. That failure edge is the only observable difference between a best-effort sync and
    /// a strict one, which is why it is pinned here.
    ///
    /// The mode denial is real only for a non-root uid; the hermetic gate runs as the sandbox's
    /// unprivileged build user, and a local root run skips the edge rather than asserting
    /// something root cannot observe.
    #[test]
    fn a_park_publication_is_owner_only_and_fails_when_its_directory_cannot_be_synced() {
        use std::os::unix::fs::PermissionsExt as _;

        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("park");
        let path = marker_path(&dir, "runtime");
        write_json_atomically(&path, &serde_json::json!({"schema": "test"}), ".park").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"{\"schema\":\"test\"}\n");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600,
            "the park marker is published owner-only"
        );

        let residue = fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(".park."))
            .collect::<Vec<_>>();
        assert!(
            residue.is_empty(),
            "staging residue left behind: {residue:?}"
        );

        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        // Write and traverse, but not read: staging and renaming still work, opening the
        // directory to sync it does not.
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o300)).unwrap();
        let refused = write_json_atomically(&path, &serde_json::json!({"schema": "test"}), ".park");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(
            refused.is_err(),
            "a directory that cannot be synced must fail the publication"
        );
    }

    fn projection(dir: &Path) -> ParkProjection {
        ParkProjection::current(dir.to_path_buf()).expect("this process has a generation")
    }

    fn parked(ids: &[&str]) -> BTreeSet<String> {
        ids.iter().map(|id| (*id).to_string()).collect()
    }

    fn desired(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|id| (*id).to_string()).collect()
    }

    #[test]
    fn same_host_supervisors_isolate_markers_and_requests_by_catalog() {
        let temp = tempfile::tempdir().unwrap();
        let catalog_a = temp.path().join("catalog-a");
        let catalog_b = temp.path().join("catalog-b");
        fs::create_dir_all(&catalog_a).unwrap();
        fs::create_dir_all(&catalog_b).unwrap();

        let channel_a =
            SupervisorScope::in_state_root(temp.path(), &catalog_a, "shared-host").unwrap();
        let channel_b =
            SupervisorScope::in_state_root(temp.path(), &catalog_b, "shared-host").unwrap();
        let projection_a = projection(&channel_a.park_dir());
        let projection_b = projection(&channel_b.park_dir());

        projection_a.publish(&parked(&["same.task"]), "crash-looped");
        projection_b.publish(&BTreeSet::new(), "crash-looped");
        let marker_a = DirParkObserver::new(channel_a.park_dir()).observe(&desired(&["same.task"]));

        request_unpark(&channel_b.unpark_request_dir(), "same.task").unwrap();
        let (taken_by_a, errors_a) = take_unpark_requests(&channel_a.unpark_request_dir());
        let (taken_by_b, errors_b) = take_unpark_requests(&channel_b.unpark_request_dir());

        assert!(errors_a.is_empty() && errors_b.is_empty());
        assert!(
            matches!(marker_a.state("same.task"), ParkState::Parked(_)),
            "catalog B deleted catalog A's same-host marker"
        );
        assert!(
            taken_by_a.is_empty(),
            "catalog A consumed catalog B's request"
        );
        assert_eq!(taken_by_b, ["same.task"]);
    }

    #[test]
    fn supervisor_scope_canonicalizes_catalog_aliases_and_includes_host() {
        let temp = tempfile::tempdir().unwrap();
        let catalog = temp.path().join("catalog");
        let alias = temp.path().join("catalog-alias");
        fs::create_dir_all(&catalog).unwrap();
        std::os::unix::fs::symlink(&catalog, &alias).unwrap();

        let direct = SupervisorScope::in_state_root(temp.path(), &catalog, "host-a").unwrap();
        let through_alias = SupervisorScope::in_state_root(temp.path(), &alias, "host-a").unwrap();
        let other_host = SupervisorScope::in_state_root(temp.path(), &catalog, "host-b").unwrap();

        assert_eq!(direct, through_alias);
        assert_ne!(direct, other_host);
    }

    #[test]
    fn published_parks_are_readable_and_clear_when_the_task_recovers() {
        let dir = tempfile::tempdir().unwrap();
        let projection = projection(dir.path());
        let observer = DirParkObserver::new(dir.path().to_path_buf());

        assert!(
            projection
                .publish(&parked(&["a", "b"]), "crash-looped")
                .is_empty()
        );
        let batch = observer.observe(&desired(&["a", "b", "healthy"]));
        assert!(
            batch.complete,
            "a parked task is a known fault, not missing evidence"
        );
        assert!(batch.errors.is_empty());
        let ParkState::Parked(record) = batch.state("a") else {
            panic!("'a' was published as parked but does not read back as parked");
        };
        assert_eq!(record.runtime_id, "a");
        assert_eq!(record.reason, "crash-looped");
        assert_eq!(record.supervisor_pid, std::process::id());
        assert!(matches!(batch.state("b"), ParkState::Parked(_)));
        assert_eq!(batch.state("healthy"), &ParkState::NotParked);

        // 'a' recovers: republishing without it must retract its marker, not leave a fault standing.
        assert!(
            projection
                .publish(&parked(&["b"]), "crash-looped")
                .is_empty()
        );
        let batch = observer.observe(&desired(&["a", "b"]));
        assert_eq!(batch.state("a"), &ParkState::NotParked);
        assert!(matches!(batch.state("b"), ParkState::Parked(_)));
        assert!(batch.complete);
    }

    /// A park belongs to one supervisor run (R31). A marker left behind by a supervisor that has since
    /// died describes a park that no longer exists, and reporting it would make `st2 tasks` claim a
    /// fault on every host where st2 merely is not running.
    ///
    /// The pid used here is this process's own with a *wrong* start time, which is exactly the
    /// pid-reuse shape: the pid is live, so a bare `kill -0` check would believe the marker.
    #[test]
    fn a_marker_from_a_dead_supervisor_reads_as_not_parked() {
        let dir = tempfile::tempdir().unwrap();
        let live = projection(dir.path());
        assert!(live.publish(&parked(&["a"]), "crash-looped").is_empty());

        let path = marker_path(dir.path(), "a");
        let mut record: ParkRecord = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        record.supervisor_start_time_ticks = record.supervisor_start_time_ticks.wrapping_add(1);
        write_json_atomically(&path, &record, ".park").unwrap();

        let batch = DirParkObserver::new(dir.path().to_path_buf()).observe(&desired(&["a"]));
        assert_eq!(
            batch.state("a"),
            &ParkState::NotParked,
            "a park outlived the supervisor run it belongs to"
        );
        assert!(
            batch.complete,
            "a stale marker is a positive absence, not an unknown"
        );
        assert!(batch.errors.is_empty());
    }

    /// Failing to observe a still-live supervisor is not evidence that its park ended. The injected
    /// observer makes this branch deterministic and kills the former `.ok()` collapse to
    /// `NotParked` without depending on host `/proc` permissions or timing.
    #[test]
    fn a_supervisor_generation_observation_error_is_indeterminate() {
        let dir = tempfile::tempdir().unwrap();
        let projection = projection(dir.path());
        assert!(
            projection
                .publish(&parked(&["a"]), "crash-looped")
                .is_empty()
        );

        let observer = DirParkObserver::new(dir.path().to_path_buf());
        let batch = observer.observe_with(&desired(&["a"]), &|_| {
            ProcessStartTimeObservation::Indeterminate("injected observation failure".into())
        });

        assert!(!batch.complete);
        assert!(matches!(batch.state("a"), ParkState::Indeterminate(_)));
        assert_eq!(batch.errors.len(), 1);
        assert!(batch.errors[0].contains("injected observation failure"));
    }

    /// Syntactically valid JSON can still carry an impossible process generation. Such evidence is
    /// malformed, not proof that a formerly owning supervisor is gone.
    #[test]
    fn malformed_supervisor_generations_are_indeterminate() {
        let dir = tempfile::tempdir().unwrap();
        let cases = [
            ("zero-pid", 0, 1),
            ("oversized-pid", u32::MAX, 1),
            ("zero-start-time", std::process::id(), 0),
        ];

        for (runtime_id, supervisor_pid, supervisor_start_time_ticks) in cases {
            let record = ParkRecord {
                schema: PARK_SCHEMA.to_string(),
                runtime_id: runtime_id.to_string(),
                supervisor_pid,
                supervisor_start_time_ticks,
                parked_at: "2026-08-09T10:00:00.000Z".to_string(),
                reason: "crash-looped".to_string(),
            };
            write_json_atomically(&marker_path(dir.path(), runtime_id), &record, ".park").unwrap();
        }

        let observer = DirParkObserver::new(dir.path().to_path_buf());
        let desired = cases.map(|(runtime_id, _, _)| runtime_id.to_string());
        let batch = observer.observe(&desired);
        assert!(!batch.complete);
        assert_eq!(batch.errors.len(), cases.len());
        for (runtime_id, _, _) in cases {
            assert!(
                matches!(batch.state(runtime_id), ParkState::Indeterminate(_)),
                "malformed generation for {runtime_id:?} was treated as a stale park"
            );
        }
    }

    /// A host that has never parked anything has no projection dir. That must read as "nothing is
    /// parked" and stay complete — otherwise every `st2 tasks` on a quiet host exits non-zero.
    #[test]
    fn an_absent_projection_is_positively_empty() {
        let dir = tempfile::tempdir().unwrap();
        let observer = DirParkObserver::new(dir.path().join("never-created"));
        let batch = observer.observe(&desired(&["a", "b"]));
        assert!(batch.complete);
        assert!(batch.errors.is_empty());
        assert_eq!(batch.state("a"), &ParkState::NotParked);
    }

    /// Unreadable evidence is not absence. A marker that exists but cannot be believed makes the batch
    /// incomplete, matching the rest of the diagnostic surface's fail-closed posture.
    #[test]
    fn an_unbelievable_marker_is_indeterminate_not_absent() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path()).unwrap();
        fs::write(marker_path(dir.path(), "garbage"), b"{not json").unwrap();
        fs::write(
            marker_path(dir.path(), "wrong-schema"),
            br#"{"schema":"st2.park.v99","runtimeId":"wrong-schema","supervisorPid":1,"supervisorStartTimeTicks":1,"parkedAt":"2026-01-01T00:00:00.000Z","reason":"x"}"#,
        )
        .unwrap();

        let batch = DirParkObserver::new(dir.path().to_path_buf())
            .observe(&desired(&["garbage", "wrong-schema"]));
        assert!(!batch.complete);
        assert_eq!(batch.errors.len(), 2);
        assert!(matches!(
            batch.state("garbage"),
            ParkState::Indeterminate(_)
        ));
        assert!(matches!(
            batch.state("wrong-schema"),
            ParkState::Indeterminate(_)
        ));
    }

    /// The park's age is what tells a fresh crash-loop from one that has been down all day, so a pass
    /// that re-publishes an existing park must not restamp it.
    #[test]
    fn republishing_an_existing_park_preserves_when_it_happened() {
        let dir = tempfile::tempdir().unwrap();
        let projection = projection(dir.path());
        projection.publish(&parked(&["a"]), "crash-looped");
        let first = fs::read(marker_path(dir.path(), "a")).unwrap();
        projection.publish(&parked(&["a"]), "crash-looped");
        assert_eq!(first, fs::read(marker_path(dir.path(), "a")).unwrap());
    }

    #[test]
    fn a_request_is_consumed_exactly_once() {
        let dir = tempfile::tempdir().unwrap();
        request_unpark(dir.path(), "b").unwrap();
        request_unpark(dir.path(), "a").unwrap();
        // Asking twice for the same task is one request, not two.
        request_unpark(dir.path(), "a").unwrap();

        let (ids, errors) = take_unpark_requests(dir.path());
        assert_eq!(ids, vec!["a".to_string(), "b".to_string()]);
        assert!(errors.is_empty());

        // Taking is consuming: a leftover request would relaunch the task on every later pass and
        // quietly disable the restart policy it was parked by.
        let (ids, errors) = take_unpark_requests(dir.path());
        assert!(ids.is_empty());
        assert!(errors.is_empty());
    }

    #[test]
    fn a_missing_request_dir_is_no_requests() {
        let dir = tempfile::tempdir().unwrap();
        let (ids, errors) = take_unpark_requests(&dir.path().join("never-created"));
        assert!(ids.is_empty());
        assert!(errors.is_empty());
    }

    /// The request dir is the one place an operator's argument becomes a path, so it is the one place
    /// that has to refuse a traversal rather than write outside itself.
    #[test]
    fn a_request_id_cannot_escape_its_dir() {
        let dir = tempfile::tempdir().unwrap();
        for bad in [
            "../escaped",
            "sub/nested",
            "/absolute",
            "",
            ".",
            "..",
            ".hidden",
        ] {
            assert!(
                request_unpark(dir.path(), bad).is_err(),
                "{bad:?} was accepted as a task id"
            );
        }
        assert!(request_unpark(dir.path(), "dev3.agent.ding").is_ok());
    }
}
