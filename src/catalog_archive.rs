//! Move retired, runtime-free identities out of the live declaration plane.
//!
//! `retired` is runtime teardown only: the declaration and its `resources/` stay byte-identical and
//! reversible. Archival is the pressure valve that keeps the live catalog bounded. An archived
//! identity's whole directory is moved under the catalog control plane, where `.st2` is already
//! excluded at any depth by discovery, validation, and the whole-catalog transaction — so an
//! archived spec is structurally undiscoverable rather than merely filtered. A tombstone beside the
//! moved directory keeps the identity traceable in `st2 catalog graph --json`, and
//! `st2 catalog unarchive` is the exact reverse move.
//!
//! Both moves run under the catalog's exclusive authoring lock and inside one generation commit,
//! and both are same-filesystem renames by construction: the archive root is a child of the catalog
//! root, so there is no copy engine and no partially copied bundle to reason about.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

use crate::CatalogLock;
use crate::catalog_lock::CONTROL_DIR;
use crate::catalog_transaction::sync_dir;
use crate::run::Runner as _;

pub const ARCHIVE_SCHEMA: &str = "st2.catalog-archive.v1";
pub const UNARCHIVE_SCHEMA: &str = "st2.catalog-unarchive.v1";
pub const TOMBSTONE_SCHEMA: &str = "st2.catalog-archive-tombstone.v1";
pub const RETIRED_LEDGER_SCHEMA: &str = "st2.catalog-retired-observed.v1";
pub const DIRECT_DEAD_LEDGER_SCHEMA: &str = "st2.catalog-direct-dead-observed.v1";

/// Archive root child of the catalog control directory.
const ARCHIVE_DIR: &str = "archive";
const TOMBSTONE_SUFFIX: &str = ".tombstone.json";
/// The supervisor's retirement-observation ledger, a sibling of the authoring lock.
const RETIRED_LEDGER_FILE: &str = "retired-observed.json";
/// The supervisor's direct-actor death-observation ledger, beside the retirement ledger.
const DIRECT_DEAD_LEDGER_FILE: &str = "direct-dead-observed.json";

const RETIRED_LEDGER: Ledger = Ledger {
    file: RETIRED_LEDGER_FILE,
    schema: RETIRED_LEDGER_SCHEMA,
};
const DIRECT_DEAD_LEDGER: Ledger = Ledger {
    file: DIRECT_DEAD_LEDGER_FILE,
    schema: DIRECT_DEAD_LEDGER_SCHEMA,
};

/// `<catalog>/.st2/archive` — the root every archived identity directory lands under.
pub fn archive_root(catalog: &Path) -> PathBuf {
    catalog.join(CONTROL_DIR).join(ARCHIVE_DIR)
}

/// Which retired identities a run addresses.
#[derive(Debug, Clone)]
pub enum Selection {
    /// Exactly these identities. Any ineligible member fails the whole run before it mutates.
    Identities(Vec<String>),
    /// Every eligible retired identity of the selected host. Ineligible ones are reported, not fatal.
    AllRetired,
}

#[derive(Debug, Clone)]
pub struct ArchiveRequest {
    pub catalog: PathBuf,
    pub host: String,
    pub selection: Selection,
    pub dry_run: bool,
}

/// The supervisor's grace-driven variant of [`ArchiveRequest`].
#[derive(Debug, Clone)]
pub struct AutoArchiveRequest {
    pub catalog: PathBuf,
    pub host: String,
    /// How long a seat must have been observed retired before it may leave. Never `ZERO`: that is
    /// the operator's off switch, checked by the caller before the pass runs at all.
    pub grace: Duration,
    /// Most seats one pass may archive. A catalog holding hundreds of retirements drains over
    /// several passes rather than one that holds the authoring lock through all of them. The pass
    /// also examines at most `limit * DUE_SCAN_FACTOR` due retired seats, and at most
    /// `remaining * DUE_SCAN_FACTOR` due dead direct actors, where `remaining` is the part of
    /// `limit` the retired seats left.
    pub limit: usize,
}

/// What one supervisor pass did: the archive result plus how far its bounded scans reached.
#[derive(Debug, Clone)]
pub struct AutoArchiveResult {
    pub archive: ArchiveResult,
    /// Due retired seats examined this pass.
    pub scanned: usize,
    /// Due retired seats left for a later pass, which resumes the scan after the last one examined.
    pub deferred: usize,
    /// Due dead direct actors examined this pass.
    pub direct_scanned: usize,
    /// Due dead direct actors left for a later pass, which resumes after the last one examined.
    pub direct_deferred: usize,
}

#[derive(Debug, Clone)]
pub struct UnarchiveRequest {
    pub catalog: PathBuf,
    pub host: String,
    pub identity: String,
}

/// The durable trace an archived identity leaves in the live catalog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tombstone {
    pub schema: String,
    pub id: String,
    pub host: String,
    pub identity: String,
    pub archived_at: u64,
    /// The retired declaration's rationale. `None` for a legacy `retired #true` declaration.
    pub reason: Option<String>,
    /// Catalog-relative location of the moved directory, so a moved catalog stays readable.
    pub archive_root: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArchivedEntry {
    pub id: String,
    pub host: String,
    pub identity: String,
    pub from: String,
    pub to: String,
    pub archived_at: u64,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Refusal {
    pub id: String,
    pub code: &'static str,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArchiveResult {
    pub schema: &'static str,
    pub host: String,
    pub archive_root: String,
    pub dry_run: bool,
    pub archived: Vec<ArchivedEntry>,
    pub refused: Vec<Refusal>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UnarchiveResult {
    pub schema: &'static str,
    pub id: String,
    pub host: String,
    pub identity: String,
    pub from: String,
    pub to: String,
    pub archived_at: Option<u64>,
}

/// What the archive root holds right now, plus every unexplained entry in it.
#[derive(Debug, Default)]
pub struct ArchiveObservation {
    pub archived: Vec<Tombstone>,
    pub issues: Vec<ArchiveIssue>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveIssue {
    /// Catalog-relative path of the unexplained entry.
    pub path: String,
    pub message: String,
}

#[derive(Debug)]
struct Candidate {
    id: String,
    host: String,
    identity: String,
    reason: Option<String>,
    from: PathBuf,
}

/// Archive every selected identity under one exclusive authoring lock and one generation commit.
pub fn archive(request: ArchiveRequest) -> Result<ArchiveResult> {
    let catalog = canonical(&request.catalog)?;
    let lock = CatalogLock::exclusive(&catalog)?;
    let found = discovered(&catalog)?;
    let (candidates, refused) = plan(&catalog, &request.host, &found, &request.selection)?;
    if let Selection::Identities(_) = request.selection
        && let Some(refusal) = refused.first()
    {
        anyhow::bail!(
            "refusing to archive {}: [{}] {}",
            refusal.id,
            refusal.code,
            refusal.message
        );
    }
    archive_locked(
        &lock,
        &catalog,
        &request.host,
        request.dry_run,
        candidates,
        refused,
    )
}

/// The supervisor's maintenance pass: archive every retired seat and every dead direct OMP actor
/// whose grace period expired.
///
/// `Ok(None)` means another authoring holder has the lock, so the pass did nothing. Skipping is
/// the point: a reconcile pass queued behind `st2 catalog apply` stalls every live agent's
/// reconciliation, and the seats are still due next pass.
pub fn auto_archive(request: AutoArchiveRequest) -> Result<Option<AutoArchiveResult>> {
    auto_archive_at(request, crate::message::now_ms())
}

fn auto_archive_at(request: AutoArchiveRequest, now_ms: u64) -> Result<Option<AutoArchiveResult>> {
    anyhow::ensure!(
        !request.grace.is_zero(),
        "auto-archive is disabled by archive-after \"0\""
    );
    validate_component("host", &request.host)?;
    let catalog = canonical(&request.catalog)?;
    let Some(lock) = CatalogLock::try_exclusive(&catalog)? else {
        return Ok(None);
    };
    let found = discovered(&catalog)?;

    // Persist the observation before archiving. A failed move then costs one batch, not every
    // seat's clock; the rows the move retires are pruned by the next pass's reconciliation.
    let mut ledger = read_ledger(&catalog, &RETIRED_LEDGER);
    let retired = retired_identities(&found.specs, &request.host);
    let (changed, due) = observe_retirements(
        &mut ledger.hosts,
        &request.host,
        &retired,
        request.grace,
        now_ms,
    );
    if changed {
        write_ledger(&catalog, &RETIRED_LEDGER, &ledger)?;
    }
    let resume_after = ledger.resume_after.get(&request.host).cloned();
    let batch = plan_due(
        &catalog,
        &request.host,
        &found,
        &due,
        resume_after.as_deref(),
        request.limit,
    )?;
    if batch.resume_after != resume_after {
        match &batch.resume_after {
            Some(identity) => ledger
                .resume_after
                .insert(request.host.clone(), identity.clone()),
            None => ledger.resume_after.remove(&request.host),
        };
        write_ledger(&catalog, &RETIRED_LEDGER, &ledger)?;
    }
    let mut candidates = batch.candidates;
    let mut refused = batch.refused;

    let direct = due_direct_actors(
        &catalog,
        &found,
        &request.host,
        request.grace,
        request.limit - candidates.len(),
        now_ms,
    )?;
    candidates.extend(direct.candidates);
    refused.extend(direct.refused);
    let archive = archive_locked(&lock, &catalog, &request.host, false, candidates, refused)?;
    Ok(Some(AutoArchiveResult {
        archive,
        scanned: batch.scanned,
        deferred: batch.deferred,
        direct_scanned: direct.scanned,
        direct_deferred: direct.deferred,
    }))
}

/// Observe this host's direct OMP actors against a fresh PTY registry read and plan the ones
/// whose death outlived `grace`, at most `limit` of them.
///
/// Death is exact evidence only: the identity decodes to one PTY session ID, and the PTY registry
/// the catalog's supervisor manages holds no running record for it. `exec` task records are not
/// PTY evidence, so one sharing the ID keeps nothing alive. A PTY that runs again drops its
/// ledger row, so a later death serves a fresh grace period. The registry read happens under the
/// exclusive lock and only when a direct actor exists, so a catalog without them pays nothing.
///
/// The scan is bounded exactly as [`plan_due`] bounds retired seats: refused actors do not
/// consume `limit`, so at most `limit * DUE_SCAN_FACTOR` due actors are examined, in identity
/// order starting after the host's cursor in the direct ledger and wrapping around.
///
/// Rows of the actors about to leave are dropped in the same ledger write: an unarchived actor
/// then serves a fresh grace period instead of leaving again on the next pass, and a failed move
/// errs toward keeping the actor live for another period.
fn due_direct_actors(
    catalog: &Path,
    found: &crate::Discovered,
    host: &str,
    grace: Duration,
    limit: usize,
    now_ms: u64,
) -> Result<DueBatch> {
    let actors = crate::direct_actor::discover(catalog, found, Some(host))?;
    let dead = if actors.is_empty() {
        BTreeSet::new()
    } else {
        let sessions = crate::run::PtyCli::new(catalog.to_path_buf())
            .list_sessions()
            .context("read PTY registry for direct actor liveness")?;
        let running: BTreeSet<String> = sessions
            .into_iter()
            .filter(|session| session.alive)
            .map(|session| session.pty_id)
            .collect();
        dead_direct_identities(&actors, &running)
    };
    let mut ledger = read_ledger(catalog, &DIRECT_DEAD_LEDGER);
    let (mut changed, due) = observe_retirements(&mut ledger.hosts, host, &dead, grace, now_ms);

    let due: BTreeSet<String> = due.into_iter().collect();
    let mut selected: Vec<crate::direct_actor::DirectActor> = actors
        .into_iter()
        .filter(|actor| due.contains(&actor.identity))
        .collect();
    selected.sort_by(|left, right| left.identity.cmp(&right.identity));
    let resume_after = ledger.resume_after.get(host).cloned();
    start_after(&mut selected, resume_after.as_deref(), |actor| {
        actor.identity.as_str()
    });
    let scan_limit = limit.saturating_mul(DUE_SCAN_FACTOR);
    let mut batch = DueBatch {
        candidates: Vec::new(),
        refused: Vec::new(),
        scanned: 0,
        deferred: 0,
        resume_after: None,
    };
    for actor in &selected {
        if batch.candidates.len() == limit || batch.scanned == scan_limit {
            break;
        }
        batch.scanned += 1;
        let occupied = archive_root(catalog).join(host).join(&actor.identity);
        if fs::symlink_metadata(&occupied).is_ok() {
            batch.refused.push(Refusal {
                id: actor.id(),
                code: "archive-occupied",
                message: format!(
                    "the archive already holds {}; unarchive it or move it aside first",
                    relative(catalog, &occupied).unwrap_or_else(|| occupied.display().to_string())
                ),
            });
            continue;
        }
        batch.candidates.push(Candidate {
            id: actor.id(),
            reason: Some(format!(
                "direct OMP actor: PTY session {} has no running record",
                actor.pty_id
            )),
            host: actor.host.clone(),
            identity: actor.identity.clone(),
            from: actor.dir.clone(),
        });
    }
    batch.deferred = selected.len() - batch.scanned;
    batch.resume_after =
        next_resume_after(&selected, batch.scanned, resume_after.as_deref(), |actor| {
            actor.identity.as_str()
        });
    if batch.resume_after != resume_after {
        match &batch.resume_after {
            Some(identity) => ledger
                .resume_after
                .insert(host.to_owned(), identity.clone()),
            None => ledger.resume_after.remove(host),
        };
        changed = true;
    }
    if let Some(observed) = ledger.hosts.get_mut(host) {
        for candidate in &batch.candidates {
            changed |= observed.remove(&candidate.identity).is_some();
        }
    }
    if changed {
        write_ledger(catalog, &DIRECT_DEAD_LEDGER, &ledger)?;
    }
    Ok(batch)
}

/// The identities of `actors` whose PTY session is not among the `running` PTY session IDs.
fn dead_direct_identities(
    actors: &[crate::direct_actor::DirectActor],
    running: &BTreeSet<String>,
) -> BTreeSet<String> {
    actors
        .iter()
        .filter(|actor| !running.contains(&actor.pty_id))
        .map(|actor| actor.identity.clone())
        .collect()
}

/// Archive planned candidates under a lock the caller already holds.
fn archive_locked(
    lock: &CatalogLock,
    catalog: &Path,
    host: &str,
    dry_run: bool,
    candidates: Vec<Candidate>,
    refused: Vec<Refusal>,
) -> Result<ArchiveResult> {
    let root = archive_root(catalog);
    let mut archived = Vec::new();
    if candidates.is_empty() || dry_run {
        for candidate in &candidates {
            archived.push(entry(catalog, candidate, 0));
        }
        return Ok(result(catalog, host, dry_run, archived, refused));
    }

    let generation = lock.begin_generation_commit()?;
    for candidate in &candidates {
        let archived_at = crate::message::now_ms();
        move_out(catalog, &root, candidate, archived_at)?;
        archived.push(entry(catalog, candidate, archived_at));
    }
    generation.commit()?;
    Ok(result(catalog, host, dry_run, archived, refused))
}

fn canonical(catalog: &Path) -> Result<PathBuf> {
    catalog
        .canonicalize()
        .with_context(|| format!("canonicalize catalog {}", catalog.display()))
}

/// Discover the whole catalog, refusing an incomplete read.
///
/// A declaration that failed to parse could be the one naming a candidate as its `supervisor`, so
/// a partial discovery must never read as "nothing depends on it".
fn discovered(catalog: &Path) -> Result<crate::Discovered> {
    let found = crate::discover_strict(catalog);
    anyhow::ensure!(
        found.errors.is_empty(),
        "refusing to archive: catalog discovery is incomplete, so a supervisor reference could be hidden:\n{}",
        found
            .errors
            .iter()
            .map(|error| format!("  {}: {}", error.path.display(), error.message))
            .collect::<Vec<_>>()
            .join("\n")
    );
    Ok(found)
}

/// Move one archived identity back into the live declaration plane.
pub fn unarchive(request: UnarchiveRequest) -> Result<UnarchiveResult> {
    let catalog = canonical(&request.catalog)?;
    validate_component("host", &request.host)?;
    validate_component("identity", &request.identity)?;
    let lock = CatalogLock::exclusive(&catalog)?;

    let host_root = archive_root(&catalog).join(&request.host);
    let from = host_root.join(&request.identity);
    let metadata = fs::symlink_metadata(&from).with_context(|| {
        format!(
            "no archived identity at {}",
            relative(&catalog, &from).unwrap_or_else(|| from.display().to_string())
        )
    })?;
    anyhow::ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "archived identity is not a real directory: {}",
        from.display()
    );
    let to = catalog
        .join("agents")
        .join(&request.host)
        .join(&request.identity);
    anyhow::ensure!(
        fs::symlink_metadata(&to).is_err(),
        "live catalog already holds {}; remove or rename it before unarchiving",
        to.display()
    );

    let tombstone_path = host_root.join(format!("{}{TOMBSTONE_SUFFIX}", request.identity));
    let archived_at = read_tombstone(&tombstone_path)
        .ok()
        .flatten()
        .map(|tombstone| tombstone.archived_at);

    let parent = to.parent().context("live identity path has no parent")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("create live host directory {}", parent.display()))?;

    let generation = lock.begin_generation_commit()?;
    fs::rename(&from, &to)
        .with_context(|| format!("restore {} to {}", from.display(), to.display()))?;
    match fs::remove_file(&tombstone_path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("remove archive tombstone {}", tombstone_path.display()));
        }
    }
    sync_dir(&host_root)?;
    sync_dir(parent)?;
    generation.commit()?;

    Ok(UnarchiveResult {
        schema: UNARCHIVE_SCHEMA,
        id: format!("{}.{}", request.host, request.identity),
        host: request.host,
        identity: request.identity,
        from: relative(&catalog, &from).unwrap_or_else(|| from.display().to_string()),
        to: relative(&catalog, &to).unwrap_or_else(|| to.display().to_string()),
        archived_at,
    })
}

/// Read every tombstone under the archive root, reporting entries that explain nothing.
///
/// The caller already holds a catalog-authoring fence. An archived directory with no readable
/// tombstone, and a tombstone with no directory, are both unexplained control-plane state: they are
/// surfaced as issues rather than silently dropped from the archived view.
pub fn observe(catalog: &Path) -> Result<ArchiveObservation> {
    let root = archive_root(catalog);
    let mut observation = ArchiveObservation::default();
    let Some(hosts) = read_real_dir_optional(&root)? else {
        return Ok(observation);
    };
    let mut tombstones: BTreeMap<String, Tombstone> = BTreeMap::new();
    for host_entry in hosts {
        let host_path = host_entry.path();
        if !host_entry.file_type()?.is_dir() {
            observation.issues.push(issue(
                catalog,
                &host_path,
                "archive root child is not a host directory",
            ));
            continue;
        }
        let host = match host_entry.file_name().to_str() {
            Some(host) => host.to_owned(),
            None => {
                observation.issues.push(issue(
                    catalog,
                    &host_path,
                    "archived host directory name is not UTF-8",
                ));
                continue;
            }
        };
        let mut directories = BTreeSet::new();
        let mut seen = BTreeSet::new();
        for entry in sorted_entries(&host_path)? {
            let path = entry.path();
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                observation
                    .issues
                    .push(issue(catalog, &path, "archived entry name is not UTF-8"));
                continue;
            };
            if let Some(identity) = name.strip_suffix(TOMBSTONE_SUFFIX) {
                match read_tombstone(&path) {
                    Ok(Some(tombstone)) if tombstone.identity == identity => {
                        seen.insert(identity.to_owned());
                        tombstones.insert(tombstone.id.clone(), tombstone);
                    }
                    Ok(_) => observation.issues.push(issue(
                        catalog,
                        &path,
                        "archive tombstone does not describe its own identity",
                    )),
                    Err(error) => observation.issues.push(issue(
                        catalog,
                        &path,
                        &format!("archive tombstone is unreadable: {error:#}"),
                    )),
                }
            } else if entry.file_type()?.is_dir() {
                directories.insert(name);
            } else {
                observation
                    .issues
                    .push(issue(catalog, &path, "unexpected archive-root file"));
            }
        }
        for identity in directories.difference(&seen) {
            observation.issues.push(issue(
                catalog,
                &host_path.join(identity),
                "archived identity has no readable tombstone",
            ));
        }
        for identity in seen.difference(&directories) {
            observation.issues.push(issue(
                catalog,
                &host_path.join(format!("{identity}{TOMBSTONE_SUFFIX}")),
                "archive tombstone has no archived identity directory",
            ));
            tombstones.remove(&format!("{host}.{identity}"));
        }
    }
    observation.archived = tombstones.into_values().collect();
    Ok(observation)
}

/// Decide, without mutating anything, which selected identities may leave the live catalog.
///
/// Every selected identity counts as leaving, so a supervisor archived together with all of its
/// dependents is eligible. The operator's verb has no per-run bound, so the whole selection is the
/// batch; the supervisor's bounded pass builds its batch with [`plan_due`] instead.
fn plan(
    catalog: &Path,
    host: &str,
    found: &crate::Discovered,
    selection: &Selection,
) -> Result<(Vec<Candidate>, Vec<Refusal>)> {
    validate_component("host", host)?;
    let mut refused = Vec::new();
    let mut selected: Vec<&agent_spec::spec::AgentSpec> = Vec::new();
    match selection {
        Selection::AllRetired => {
            for spec in &found.specs {
                if spec.resolved_host(host) == host && spec.desired_state.is_retired() {
                    selected.push(spec);
                }
            }
        }
        Selection::Identities(identities) => {
            for identity in identities {
                validate_component("identity", identity)?;
                let matches = found
                    .specs
                    .iter()
                    .filter(|spec| spec.resolved_host(host) == host && spec.identity == *identity)
                    .collect::<Vec<_>>();
                match matches.as_slice() {
                    [spec] => selected.push(spec),
                    [] => refused.push(Refusal {
                        id: format!("{host}.{identity}"),
                        code: "unknown-identity",
                        message: format!("no declaration for '{identity}' on host '{host}'"),
                    }),
                    many => refused.push(Refusal {
                        id: format!("{host}.{identity}"),
                        code: "ambiguous-identity",
                        message: format!("{} declarations claim this identity", many.len()),
                    }),
                }
            }
        }
    }
    selected.sort_by(|left, right| left.identity.cmp(&right.identity));
    let mut candidates = Vec::new();
    if !selected.is_empty() {
        let records = runtime_records(catalog, host)?;
        let leaving: BTreeSet<&Path> = selected.iter().map(|spec| spec.path.as_path()).collect();
        for spec in &selected {
            match eligibility(catalog, host, found, &records, &leaving, spec) {
                Ok(candidate) => candidates.push(candidate),
                Err(refusal) => refused.push(refusal),
            }
        }
    }
    refused.sort_by(|left, right| left.id.cmp(&right.id));
    Ok((candidates, refused))
}

/// How many due retired seats (or dead direct actors) one supervisor pass may examine, as a
/// multiple of its archive limit. Refused ones do not consume the limit, so without this bound a
/// catalog full of them would make one pass under the exclusive lock scan every due one.
const DUE_SCAN_FACTOR: usize = 4;

/// The supervisor's bounded batch of grace-expired retired seats or dead direct actors.
struct DueBatch {
    candidates: Vec<Candidate>,
    refused: Vec<Refusal>,
    /// Due entries examined this pass: at most `limit * DUE_SCAN_FACTOR`.
    scanned: usize,
    /// Due entries this pass left unexamined because it reached its limit or its scan bound.
    deferred: usize,
    /// The identity the next pass's scan resumes after; `None` once a scan reached every due one.
    resume_after: Option<String>,
}

/// Rotate identity-sorted `items` so the scan starts after `resume_after`, wrapping around.
fn start_after<T>(items: &mut [T], resume_after: Option<&str>, identity: impl Fn(&T) -> &str) {
    if let Some(after) = resume_after {
        let start = items.partition_point(|item| identity(item) <= after);
        items.rotate_left(start);
    }
}

/// Where the next pass resumes after a scan examined the first `scanned` of the rotated `items`:
/// the last one examined, the unchanged cursor when nothing was examined, or `None` once the scan
/// reached every item.
fn next_resume_after<T>(
    items: &[T],
    scanned: usize,
    resume_after: Option<&str>,
    identity: impl Fn(&T) -> &str,
) -> Option<String> {
    if scanned == items.len() {
        return None;
    }
    match scanned.checked_sub(1) {
        Some(last) => Some(identity(&items[last]).to_owned()),
        None => resume_after.map(str::to_owned),
    }
}

/// Build the supervisor's batch from the `due` identities, bounded in both archived and examined
/// seats.
///
/// Seats are walked in identity order, starting after `resume_after` and wrapping around, so a run
/// of seats refused every pass cannot hold the scan window: the next pass resumes where this one
/// stopped. Each seat is judged against the batch accepted so far, which is the only set leaving
/// this pass; a supervisor whose retired dependent has not been accepted yet is refused and leaves
/// on a later pass, after the dependent. Refused seats do not consume `limit`.
fn plan_due(
    catalog: &Path,
    host: &str,
    found: &crate::Discovered,
    due: &[String],
    resume_after: Option<&str>,
    limit: usize,
) -> Result<DueBatch> {
    let due: BTreeSet<&str> = due.iter().map(String::as_str).collect();
    let mut selected: Vec<&agent_spec::spec::AgentSpec> = found
        .specs
        .iter()
        .filter(|spec| {
            spec.resolved_host(host) == host
                && spec.desired_state.is_retired()
                && due.contains(spec.identity.as_str())
        })
        .collect();
    selected.sort_by(|left, right| left.identity.cmp(&right.identity));
    start_after(&mut selected, resume_after, |spec| spec.identity.as_str());
    let mut batch = DueBatch {
        candidates: Vec::new(),
        refused: Vec::new(),
        scanned: 0,
        deferred: selected.len(),
        resume_after: None,
    };
    if selected.is_empty() {
        return Ok(batch);
    }

    let records = runtime_records(catalog, host)?;
    let scan_limit = limit.saturating_mul(DUE_SCAN_FACTOR);
    let mut accepted: Vec<(&agent_spec::spec::AgentSpec, Candidate)> = Vec::new();
    let mut leaving: BTreeSet<&Path> = BTreeSet::new();
    for spec in &selected {
        if accepted.len() == limit || batch.scanned == scan_limit {
            break;
        }
        batch.scanned += 1;
        match eligibility(catalog, host, found, &records, &leaving, spec) {
            Ok(candidate) => {
                leaving.insert(spec.path.as_path());
                accepted.push((spec, candidate));
            }
            Err(refusal) => batch.refused.push(refusal),
        }
    }
    // The gate that keeps a supervisor from leaving ahead of its dependents, re-checked against
    // the final batch: a supervisor accepted here must leave with every dependent it still has.
    while let Some((index, dependents)) =
        accepted.iter().enumerate().find_map(|(index, (spec, _))| {
            let dependents = dependents(found, host, spec, &leaving);
            (!dependents.is_empty()).then_some((index, dependents))
        })
    {
        let (spec, candidate) = accepted.remove(index);
        leaving.remove(spec.path.as_path());
        batch
            .refused
            .push(supervisor_referenced(candidate.id, &dependents));
    }

    batch.deferred = selected.len() - batch.scanned;
    batch.resume_after = next_resume_after(&selected, batch.scanned, resume_after, |spec| {
        spec.identity.as_str()
    });
    batch.candidates = accepted
        .into_iter()
        .map(|(_, candidate)| candidate)
        .collect();
    batch.refused.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(batch)
}

/// Task runtime ID → alive, from the host's runtime registries.
///
/// Only read with a candidate in hand: the snapshot costs a `pty list` subprocess, and the
/// supervisor's pass asks this question on every tick.
fn runtime_records(catalog: &Path, host: &str) -> Result<BTreeMap<String, bool>> {
    let runner =
        crate::run::SystemRunner::new(catalog.to_path_buf(), crate::run::exec_state_dir(host));
    Ok(runner
        .list_sessions()
        .context("read task runtime records for archive eligibility")?
        .into_iter()
        .map(|session| (session.pty_id, session.alive))
        .collect())
}

/// Whether one selected declaration may leave together with `leaving`.
///
/// Eligibility is fail-closed on every axis: the identity is discovered at its canonical path on
/// the selected host, its declaration is retired in either spelling, no runtime record of any of
/// its declared tasks exists (the rule `st2 doctor` already applies to retirement), no declaration
/// outside `leaving` names it as `supervisor`, and its archive slot is free.
fn eligibility(
    catalog: &Path,
    host: &str,
    found: &crate::Discovered,
    records: &BTreeMap<String, bool>,
    leaving: &BTreeSet<&Path>,
    spec: &agent_spec::spec::AgentSpec,
) -> std::result::Result<Candidate, Refusal> {
    let id = spec.bus_id(host);
    let identity = spec.identity.clone();
    let from = catalog.join("agents").join(host).join(&identity);
    if spec.path != from.join("agent.kdl") {
        return Err(Refusal {
            id,
            code: "non-canonical-declaration",
            message: format!(
                "declaration is at {}, not the canonical agents/{host}/{identity}/agent.kdl",
                relative(catalog, &spec.path).unwrap_or_else(|| spec.path.display().to_string())
            ),
        });
    }
    if !spec.desired_state.is_retired() {
        return Err(Refusal {
            id,
            code: "not-retired",
            message: format!(
                "desired state is '{}'; archive requires 'retired'",
                spec.desired_state.as_str()
            ),
        });
    }
    let live = spec
        .tasks
        .iter()
        .map(|task| {
            task.id
                .clone()
                .unwrap_or_else(|| format!("{id}.{}", task.name))
        })
        .filter_map(|task_id| {
            records
                .get(&task_id)
                .map(|alive| format!("{task_id} ({})", if *alive { "alive" } else { "dead" }))
        })
        .collect::<Vec<_>>();
    if !live.is_empty() {
        return Err(Refusal {
            id,
            code: "runtime-record-present",
            message: format!("retirement is incomplete: {}", live.join(", ")),
        });
    }
    let dependents = dependents(found, host, spec, leaving);
    if !dependents.is_empty() {
        return Err(supervisor_referenced(id, &dependents));
    }
    // A declaration re-created under a name the archive still holds — a `catalog apply` after
    // an archival, say. Refusing it as one skip keeps the rest of the batch moving; letting
    // `move_out` fail instead would abort every remaining candidate on every pass.
    let occupied = archive_root(catalog).join(host).join(&identity);
    if fs::symlink_metadata(&occupied).is_ok() {
        return Err(Refusal {
            id,
            code: "archive-occupied",
            message: format!(
                "the archive already holds {}; unarchive it or move it aside first",
                relative(catalog, &occupied).unwrap_or_else(|| occupied.display().to_string())
            ),
        });
    }
    Ok(Candidate {
        id,
        host: host.to_owned(),
        identity,
        reason: spec.desired_state.reason().map(str::to_owned),
        from,
    })
}

/// Bus IDs of the declarations outside `leaving` that name `spec` as their `supervisor`.
fn dependents(
    found: &crate::Discovered,
    host: &str,
    spec: &agent_spec::spec::AgentSpec,
    leaving: &BTreeSet<&Path>,
) -> Vec<String> {
    found
        .specs
        .iter()
        .filter(|other| !leaving.contains(other.path.as_path()))
        .filter(|other| {
            other.supervisor.as_deref().is_some_and(|supervisor| {
                crate::supervisor_chain::resolve_spec(
                    &found.specs,
                    supervisor,
                    other.resolved_host(host),
                )
                .is_some_and(|resolved| resolved.path == spec.path)
            })
        })
        .map(|other| other.bus_id(host))
        .collect()
}

fn supervisor_referenced(id: String, dependents: &[String]) -> Refusal {
    Refusal {
        id,
        code: "supervisor-referenced",
        message: format!("still supervises {}", dependents.join(", ")),
    }
}

/// Rename the identity directory into the archive root, then record its tombstone.
///
/// The rename lands first on purpose. A crash between the two steps leaves an archived directory
/// with no tombstone, which `observe` reports and `st2 catalog unarchive` still reverses; the
/// opposite order would leave a tombstone advertising an identity that never moved.
fn move_out(catalog: &Path, root: &Path, candidate: &Candidate, archived_at: u64) -> Result<()> {
    let host_root = root.join(&candidate.host);
    fs::create_dir_all(&host_root)
        .with_context(|| format!("create archive host directory {}", host_root.display()))?;
    let to = host_root.join(&candidate.identity);
    let tombstone_path = host_root.join(format!("{}{TOMBSTONE_SUFFIX}", candidate.identity));
    anyhow::ensure!(
        fs::symlink_metadata(&to).is_err(),
        "archive already holds {}; unarchive or move it aside first",
        to.display()
    );
    anyhow::ensure!(
        fs::symlink_metadata(&tombstone_path).is_err(),
        "archive already holds a tombstone at {}",
        tombstone_path.display()
    );

    let from_parent = candidate
        .from
        .parent()
        .context("canonical identity path has no host directory")?;
    fs::rename(&candidate.from, &to).with_context(|| {
        format!(
            "move {} to {} (the archive root must share the catalog's filesystem)",
            candidate.from.display(),
            to.display()
        )
    })?;
    sync_dir(from_parent)?;

    let tombstone = Tombstone {
        schema: TOMBSTONE_SCHEMA.to_owned(),
        id: candidate.id.clone(),
        host: candidate.host.clone(),
        identity: candidate.identity.clone(),
        archived_at,
        reason: candidate.reason.clone(),
        archive_root: relative(catalog, &to).context("archive destination escaped the catalog")?,
    };
    let mut body = serde_json::to_vec_pretty(&tombstone)?;
    body.push(b'\n');
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(&tombstone_path)
        .with_context(|| format!("create archive tombstone {}", tombstone_path.display()))?;
    file.write_all(&body)?;
    file.sync_all()?;
    sync_dir(&host_root)?;
    Ok(())
}

fn entry(catalog: &Path, candidate: &Candidate, archived_at: u64) -> ArchivedEntry {
    let to = archive_root(catalog)
        .join(&candidate.host)
        .join(&candidate.identity);
    ArchivedEntry {
        id: candidate.id.clone(),
        host: candidate.host.clone(),
        identity: candidate.identity.clone(),
        from: relative(catalog, &candidate.from)
            .unwrap_or_else(|| candidate.from.display().to_string()),
        to: relative(catalog, &to).unwrap_or_else(|| to.display().to_string()),
        archived_at,
        reason: candidate.reason.clone(),
    }
}

fn result(
    catalog: &Path,
    host: &str,
    dry_run: bool,
    archived: Vec<ArchivedEntry>,
    refused: Vec<Refusal>,
) -> ArchiveResult {
    let root = archive_root(catalog);
    ArchiveResult {
        schema: ARCHIVE_SCHEMA,
        host: host.to_owned(),
        archive_root: relative(catalog, &root).unwrap_or_else(|| root.display().to_string()),
        dry_run,
        archived,
        refused,
    }
}

// ---- Observation ledgers ----------------------------------------------------------------------

/// When this catalog's supervisor first observed each archive-bound identity, per host.
///
/// st2 records nothing when a desired state changes: the declaration is rewritten in place, the
/// receipt goes to the caller's stdout, and the generation counter carries no per-identity data.
/// So the grace period is measured from the supervisor's first observation of the retirement
/// rather than from the edit that caused it. That observation is control-plane state under
/// `.st2/` and never enters the spec — `retired` keeps every declared byte reversible, which is
/// the guarantee archival is built on top of. A direct OMP actor has no declaration at all, so
/// its death is measured the same way, in its own ledger.
///
/// Hosts are separate maps because one catalog may declare several, and a supervisor can only
/// observe its own.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ObservationLedger {
    #[serde(default)]
    schema: String,
    /// host → identity → epoch millis of the first observation.
    #[serde(default)]
    hosts: BTreeMap<String, BTreeMap<String, u64>>,
    /// host → the identity the supervisor's bounded scan of due seats (or due dead direct actors)
    /// resumes after. Present only while the last pass left due ones unexamined.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    resume_after: BTreeMap<String, String>,
}

/// Which observation ledger a read or write addresses.
struct Ledger {
    file: &'static str,
    schema: &'static str,
}

/// `<catalog>/.st2/retired-observed.json`.
pub fn retired_ledger_path(catalog: &Path) -> PathBuf {
    catalog.join(CONTROL_DIR).join(RETIRED_LEDGER_FILE)
}

/// `<catalog>/.st2/direct-dead-observed.json`.
pub fn direct_dead_ledger_path(catalog: &Path) -> PathBuf {
    catalog.join(CONTROL_DIR).join(DIRECT_DEAD_LEDGER_FILE)
}

/// Read a ledger, treating an absent, unreadable, or foreign-schema file as empty.
///
/// A lost ledger restarts every grace period, which errs toward keeping identities in the live
/// catalog — the recoverable direction. Refusing the pass instead would let one unreadable control
/// file stop reconciliation.
fn read_ledger(catalog: &Path, ledger: &Ledger) -> ObservationLedger {
    let empty = || ObservationLedger {
        schema: ledger.schema.to_owned(),
        ..ObservationLedger::default()
    };
    let Ok(body) = fs::read(catalog.join(CONTROL_DIR).join(ledger.file)) else {
        return empty();
    };
    match serde_json::from_slice::<ObservationLedger>(&body) {
        Ok(read) if read.schema == ledger.schema => read,
        _ => empty(),
    }
}

/// Replace a ledger atomically with `contents`, as [`read_ledger`] returned and the caller then
/// updated. The caller holds the exclusive authoring lock.
fn write_ledger(catalog: &Path, ledger: &Ledger, contents: &ObservationLedger) -> Result<()> {
    let mut body = serde_json::to_vec_pretty(contents)?;
    body.push(b'\n');

    let path = catalog.join(CONTROL_DIR).join(ledger.file);
    let control = path.parent().context("ledger path has no control dir")?;
    let staged = control.join(format!("{}.new", ledger.file));
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(&staged)
        .with_context(|| format!("stage observation ledger {}", staged.display()))?;
    file.write_all(&body)?;
    file.sync_all()?;
    drop(file);
    if let Err(error) = fs::rename(&staged, &path) {
        let _ = fs::remove_file(&staged);
        return Err(error)
            .with_context(|| format!("install observation ledger {}", path.display()));
    }
    sync_dir(control)
}

/// The local host's retired identities as this discovery sees them.
fn retired_identities(specs: &[agent_spec::spec::AgentSpec], host: &str) -> BTreeSet<String> {
    specs
        .iter()
        .filter(|spec| spec.resolved_host(host) == host && spec.desired_state.is_retired())
        .map(|spec| spec.identity.clone())
        .collect()
}

/// Fold one observation into a ledger and report which identities have outlived `grace`.
///
/// The host's map is reconciled to exactly `retired`: a seat that came back (or a direct actor
/// whose PTY runs again) drops its entry, so a second retirement starts a fresh clock rather than
/// inheriting the old one. Returns whether the ledger changed (and therefore needs persisting)
/// alongside the grace-expired identities.
fn observe_retirements(
    hosts: &mut BTreeMap<String, BTreeMap<String, u64>>,
    host: &str,
    retired: &BTreeSet<String>,
    grace: Duration,
    now_ms: u64,
) -> (bool, Vec<String>) {
    let grace_ms = u64::try_from(grace.as_millis()).unwrap_or(u64::MAX);
    let observed = hosts.entry(host.to_owned()).or_default();
    let before = observed.len();
    observed.retain(|identity, _| retired.contains(identity));
    let mut changed = observed.len() != before;
    for identity in retired {
        if !observed.contains_key(identity) {
            observed.insert(identity.clone(), now_ms);
            changed = true;
        }
    }
    let due = observed
        .iter()
        .filter(|(_, observed_at)| now_ms.saturating_sub(**observed_at) >= grace_ms)
        .map(|(identity, _)| identity.clone())
        .collect();
    (changed, due)
}

/// Does the supervisor's archive step have anything to do this pass?
///
/// Answered from the discovery and PTY registry snapshot the pass already made, one two-level
/// directory read, and two small JSON reads, so a steady catalog pays neither a second discovery
/// nor a second `pty list`. `running_pty` holds the IDs the PTY backend alone reported running;
/// `exec` records are not direct actor liveness. Deliberately advisory: the decision that moves
/// bytes is re-made under the exclusive lock, because a seat can be un-retired, or a PTY
/// restarted, between this question and that lock.
pub fn pass_has_work(
    catalog: &Path,
    host: &str,
    found: &crate::Discovered,
    running_pty: &BTreeSet<String>,
    grace: Duration,
) -> bool {
    pass_has_work_at(
        catalog,
        host,
        found,
        running_pty,
        grace,
        crate::message::now_ms(),
    )
}

fn pass_has_work_at(
    catalog: &Path,
    host: &str,
    found: &crate::Discovered,
    running_pty: &BTreeSet<String>,
    grace: Duration,
    now_ms: u64,
) -> bool {
    let retired = retired_identities(&found.specs, host);
    let mut hosts = read_ledger(catalog, &RETIRED_LEDGER).hosts;
    let (changed, due) = observe_retirements(&mut hosts, host, &retired, grace, now_ms);
    if changed || !due.is_empty() {
        return true;
    }
    // An unreadable `agents/` tree is left to the locked step, which reports it as a warning.
    let Ok(actors) = crate::direct_actor::discover(catalog, found, Some(host)) else {
        return true;
    };
    let dead = dead_direct_identities(&actors, running_pty);
    let mut hosts = read_ledger(catalog, &DIRECT_DEAD_LEDGER).hosts;
    let (changed, due) = observe_retirements(&mut hosts, host, &dead, grace, now_ms);
    changed || !due.is_empty()
}

fn read_tombstone(path: &Path) -> Result<Option<Tombstone>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("inspect archive tombstone {}", path.display()));
        }
    };
    anyhow::ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "archive tombstone is not a real regular file"
    );
    let bytes =
        fs::read(path).with_context(|| format!("read archive tombstone {}", path.display()))?;
    let tombstone: Tombstone = serde_json::from_slice(&bytes).context("parse archive tombstone")?;
    anyhow::ensure!(
        tombstone.schema == TOMBSTONE_SCHEMA,
        "unknown archive tombstone schema '{}'",
        tombstone.schema
    );
    anyhow::ensure!(
        tombstone.id == format!("{}.{}", tombstone.host, tombstone.identity),
        "archive tombstone id does not match its host and identity"
    );
    Ok(Some(tombstone))
}

fn issue(catalog: &Path, path: &Path, message: &str) -> ArchiveIssue {
    ArchiveIssue {
        path: relative(catalog, path).unwrap_or_else(|| path.display().to_string()),
        message: message.to_owned(),
    }
}

fn relative(catalog: &Path, path: &Path) -> Option<String> {
    Some(path.strip_prefix(catalog).ok()?.display().to_string())
}

fn read_real_dir_optional(path: &Path) -> Result<Option<Vec<fs::DirEntry>>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            anyhow::ensure!(
                metadata.is_dir() && !metadata.file_type().is_symlink(),
                "archive root is not a real directory: {}",
                path.display()
            );
            Ok(Some(sorted_entries(path)?))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => {
            Err(error).with_context(|| format!("inspect archive root {}", path.display()))
        }
    }
}

fn sorted_entries(path: &Path) -> Result<Vec<fs::DirEntry>> {
    let mut entries = fs::read_dir(path)
        .with_context(|| format!("read directory {}", path.display()))?
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| format!("read directory entries {}", path.display()))?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    Ok(entries)
}

fn validate_component(label: &str, value: &str) -> Result<()> {
    anyhow::ensure!(
        !value.is_empty()
            && !value.contains('/')
            && !value.contains('\0')
            && !matches!(value, "." | ".." | ".git" | ".st2"),
        "{label} is not one safe path component: {value:?}"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOST: &str = "h";
    const DAY: u64 = 24 * 60 * 60 * 1000;

    fn retired(identities: &[&str]) -> BTreeSet<String> {
        identities.iter().map(|id| (*id).to_owned()).collect()
    }

    #[test]
    fn a_first_observation_starts_the_clock_without_reporting_the_seat_due() {
        let mut hosts = BTreeMap::new();
        let (changed, due) = observe_retirements(
            &mut hosts,
            HOST,
            &retired(&["gone"]),
            Duration::from_millis(7 * DAY),
            10 * DAY,
        );
        assert!(changed, "a new retirement must be persisted");
        assert!(due.is_empty(), "{due:?}");
        assert_eq!(hosts[HOST]["gone"], 10 * DAY);
    }

    #[test]
    fn an_unchanged_observation_reports_no_write_and_nothing_due() {
        let mut hosts =
            BTreeMap::from([(HOST.to_owned(), BTreeMap::from([("gone".to_owned(), 0)]))]);
        let (changed, due) = observe_retirements(
            &mut hosts,
            HOST,
            &retired(&["gone"]),
            Duration::from_millis(7 * DAY),
            DAY,
        );
        assert!(!changed, "a steady catalog must not rewrite the ledger");
        assert!(due.is_empty(), "{due:?}");
    }

    #[test]
    fn a_retirement_older_than_the_grace_period_is_due() {
        let mut hosts =
            BTreeMap::from([(HOST.to_owned(), BTreeMap::from([("gone".to_owned(), 0)]))]);
        let (changed, due) = observe_retirements(
            &mut hosts,
            HOST,
            &retired(&["gone"]),
            Duration::from_millis(7 * DAY),
            7 * DAY,
        );
        assert!(!changed);
        assert_eq!(due, vec!["gone".to_owned()]);
    }

    #[test]
    fn un_retiring_a_seat_drops_its_entry_so_re_retiring_restarts_the_clock() {
        let mut hosts =
            BTreeMap::from([(HOST.to_owned(), BTreeMap::from([("back".to_owned(), 0)]))]);
        let grace = Duration::from_millis(7 * DAY);

        let (changed, due) = observe_retirements(&mut hosts, HOST, &retired(&[]), grace, 8 * DAY);
        assert!(changed, "the stale entry must be pruned");
        assert!(due.is_empty(), "{due:?}");

        let (changed, due) =
            observe_retirements(&mut hosts, HOST, &retired(&["back"]), grace, 9 * DAY);
        assert!(changed);
        assert!(
            due.is_empty(),
            "the second retirement must serve its own grace period, not the first one's: {due:?}"
        );
        assert_eq!(hosts[HOST]["back"], 9 * DAY);
    }

    #[test]
    fn another_hosts_observations_are_untouched() {
        let mut hosts = BTreeMap::from([(
            "other".to_owned(),
            BTreeMap::from([("theirs".to_owned(), 0)]),
        )]);
        let (_, due) = observe_retirements(
            &mut hosts,
            HOST,
            &retired(&[]),
            Duration::from_millis(DAY),
            9 * DAY,
        );
        assert!(due.is_empty(), "{due:?}");
        assert_eq!(
            hosts["other"]["theirs"], 0,
            "only the local supervisor may reconcile its own host's rows"
        );
    }

    #[test]
    fn only_the_exact_running_pty_id_keeps_a_direct_actor_alive() {
        let actor = |segment: &str| crate::direct_actor::DirectActor {
            host: HOST.to_owned(),
            identity: format!("direct.omp.{segment}"),
            pty_id: crate::direct_actor::decode_pty_segment(segment).unwrap(),
            dir: PathBuf::from(format!("/catalog/agents/h/direct.omp.{segment}")),
        };
        let actors = [
            actor("e2jcd9pf"),
            actor("2ahzpbs3"),
            actor("x-776562"),
            actor("3bk3wt7v"),
        ];
        // A running session whose ID merely contains the actor's is not its session.
        let running = retired(&["e2jcd9pf", "web", "3bk3wt7v-child"]);
        assert_eq!(
            dead_direct_identities(&actors, &running),
            retired(&["direct.omp.2ahzpbs3", "direct.omp.3bk3wt7v"])
        );
    }
}
