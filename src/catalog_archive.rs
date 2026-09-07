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

/// Archive root child of the catalog control directory.
const ARCHIVE_DIR: &str = "archive";
const TOMBSTONE_SUFFIX: &str = ".tombstone.json";
/// The supervisor's retirement-observation ledger, a sibling of the authoring lock.
const RETIRED_LEDGER_FILE: &str = "retired-observed.json";

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
    /// The grace-expired subset the supervisor decided on: `AllRetired` narrowed to these
    /// identities, so an ineligible one is reported and skipped rather than fatal.
    Due(Vec<String>),
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
    /// several passes rather than one that holds the authoring lock through all of them.
    pub limit: usize,
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
    archive_locked(
        &lock,
        &catalog,
        &found,
        &request.host,
        &request.selection,
        request.dry_run,
    )
}

/// The supervisor's maintenance pass: archive every retired seat whose grace period expired.
///
/// `Ok(None)` means another authoring holder has the lock, so the pass did nothing. Skipping is
/// the point: a reconcile pass queued behind `st2 catalog apply` stalls every live agent's
/// reconciliation, and the seats are still due next pass.
pub fn auto_archive(request: AutoArchiveRequest) -> Result<Option<ArchiveResult>> {
    auto_archive_at(request, crate::message::now_ms())
}

fn auto_archive_at(request: AutoArchiveRequest, now_ms: u64) -> Result<Option<ArchiveResult>> {
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
    let mut hosts = read_ledger(&catalog);
    let retired = retired_identities(&found.specs, &request.host);
    let (changed, mut due) =
        observe_retirements(&mut hosts, &request.host, &retired, request.grace, now_ms);
    if changed {
        write_ledger(&catalog, hosts)?;
    }

    due.truncate(request.limit);
    archive_locked(
        &lock,
        &catalog,
        &found,
        &request.host,
        &Selection::Due(due),
        false,
    )
    .map(Some)
}

/// Archive under a lock the caller already holds, against a discovery it already made.
fn archive_locked(
    lock: &CatalogLock,
    catalog: &Path,
    found: &crate::Discovered,
    host: &str,
    selection: &Selection,
    dry_run: bool,
) -> Result<ArchiveResult> {
    let (candidates, refused) = plan(catalog, host, found, selection)?;

    if let Selection::Identities(_) = selection
        && let Some(refusal) = refused.first()
    {
        anyhow::bail!(
            "refusing to archive {}: [{}] {}",
            refusal.id,
            refusal.code,
            refusal.message
        );
    }

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
/// Eligibility is fail-closed on every axis: the identity is discovered at its canonical path on
/// the selected host, its declaration is retired in either spelling, no runtime record of any of
/// its declared tasks exists (the rule `st2 doctor` already applies to retirement), and no
/// declaration that stays behind names it as `supervisor`.
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
        Selection::Due(identities) => {
            let due: BTreeSet<&str> = identities.iter().map(String::as_str).collect();
            for spec in &found.specs {
                if spec.resolved_host(host) == host
                    && spec.desired_state.is_retired()
                    && due.contains(spec.identity.as_str())
                {
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
    if selected.is_empty() {
        refused.sort_by(|left, right| left.id.cmp(&right.id));
        return Ok((Vec::new(), refused));
    }

    // Only reached with a candidate in hand: the runtime snapshot costs a `pty list` subprocess,
    // and the supervisor's pass asks this question on every tick.
    let runner =
        crate::run::SystemRunner::new(catalog.to_path_buf(), crate::run::exec_state_dir(host));
    let records: BTreeMap<String, bool> = runner
        .list_sessions()
        .context("read task runtime records for archive eligibility")?
        .into_iter()
        .map(|session| (session.pty_id, session.alive))
        .collect();

    let leaving: BTreeSet<&Path> = selected.iter().map(|spec| spec.path.as_path()).collect();
    let mut candidates = Vec::new();
    for spec in &selected {
        let id = spec.bus_id(host);
        let identity = spec.identity.clone();
        let from = catalog.join("agents").join(host).join(&identity);
        if spec.path != from.join("agent.kdl") {
            refused.push(Refusal {
                id,
                code: "non-canonical-declaration",
                message: format!(
                    "declaration is at {}, not the canonical agents/{host}/{identity}/agent.kdl",
                    relative(catalog, &spec.path)
                        .unwrap_or_else(|| spec.path.display().to_string())
                ),
            });
            continue;
        }
        if !spec.desired_state.is_retired() {
            refused.push(Refusal {
                id,
                code: "not-retired",
                message: format!(
                    "desired state is '{}'; archive requires 'retired'",
                    spec.desired_state.as_str()
                ),
            });
            continue;
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
            refused.push(Refusal {
                id,
                code: "runtime-record-present",
                message: format!("retirement is incomplete: {}", live.join(", ")),
            });
            continue;
        }
        let dependents = found
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
            .collect::<Vec<_>>();
        if !dependents.is_empty() {
            refused.push(Refusal {
                id,
                code: "supervisor-referenced",
                message: format!("still supervises {}", dependents.join(", ")),
            });
            continue;
        }
        // A declaration re-created under a name the archive still holds — a `catalog apply` after
        // an archival, say. Refusing it as one skip keeps the rest of the batch moving; letting
        // `move_out` fail instead would abort every remaining candidate on every pass.
        let occupied = archive_root(catalog).join(host).join(&identity);
        if fs::symlink_metadata(&occupied).is_ok() {
            refused.push(Refusal {
                id,
                code: "archive-occupied",
                message: format!(
                    "the archive already holds {}; unarchive it or move it aside first",
                    relative(catalog, &occupied).unwrap_or_else(|| occupied.display().to_string())
                ),
            });
            continue;
        }
        candidates.push(Candidate {
            id,
            host: host.to_owned(),
            identity,
            reason: spec.desired_state.reason().map(str::to_owned),
            from,
        });
    }
    refused.sort_by(|left, right| left.id.cmp(&right.id));
    Ok((candidates, refused))
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

// ---- Retirement observation ledger ------------------------------------------------------------

/// When this catalog's supervisor first observed each retired seat, per host.
///
/// st2 records nothing when a desired state changes: the declaration is rewritten in place, the
/// receipt goes to the caller's stdout, and the generation counter carries no per-identity data.
/// So the grace period is measured from the supervisor's first observation of the retirement
/// rather than from the edit that caused it. That observation is control-plane state under
/// `.st2/` and never enters the spec — `retired` keeps every declared byte reversible, which is
/// the guarantee archival is built on top of.
///
/// Hosts are separate maps because one catalog may declare several, and a supervisor can only
/// observe its own.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RetiredLedger {
    #[serde(default)]
    schema: String,
    /// host → identity → epoch millis of the first observation.
    #[serde(default)]
    hosts: BTreeMap<String, BTreeMap<String, u64>>,
}

/// `<catalog>/.st2/retired-observed.json`.
pub fn retired_ledger_path(catalog: &Path) -> PathBuf {
    catalog.join(CONTROL_DIR).join(RETIRED_LEDGER_FILE)
}

/// Read the ledger, treating an absent, unreadable, or foreign-schema file as empty.
///
/// A lost ledger restarts every grace period, which errs toward keeping seats in the live catalog
/// — the recoverable direction. Refusing the pass instead would let one unreadable control file
/// stop reconciliation.
fn read_ledger(catalog: &Path) -> BTreeMap<String, BTreeMap<String, u64>> {
    let Ok(body) = fs::read(retired_ledger_path(catalog)) else {
        return BTreeMap::new();
    };
    match serde_json::from_slice::<RetiredLedger>(&body) {
        Ok(ledger) if ledger.schema == RETIRED_LEDGER_SCHEMA => ledger.hosts,
        _ => BTreeMap::new(),
    }
}

/// Replace the ledger atomically. The caller holds the exclusive authoring lock.
fn write_ledger(catalog: &Path, hosts: BTreeMap<String, BTreeMap<String, u64>>) -> Result<()> {
    let ledger = RetiredLedger {
        schema: RETIRED_LEDGER_SCHEMA.to_owned(),
        hosts,
    };
    let mut body = serde_json::to_vec_pretty(&ledger)?;
    body.push(b'\n');

    let path = retired_ledger_path(catalog);
    let control = path.parent().context("ledger path has no control dir")?;
    let staged = control.join(format!("{RETIRED_LEDGER_FILE}.new"));
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(&staged)
        .with_context(|| format!("stage retirement ledger {}", staged.display()))?;
    file.write_all(&body)?;
    file.sync_all()?;
    drop(file);
    if let Err(error) = fs::rename(&staged, &path) {
        let _ = fs::remove_file(&staged);
        return Err(error).with_context(|| format!("install retirement ledger {}", path.display()));
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

/// Fold one observation into the ledger and report which seats have outlived `grace`.
///
/// The host's map is reconciled to exactly `retired`: a seat that came back drops its entry, so
/// re-retiring it starts a fresh clock rather than inheriting the old one. Returns whether the
/// ledger changed (and therefore needs persisting) alongside the grace-expired identities.
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
/// Answered from the specs the pass already parsed plus one small JSON read, so a steady catalog
/// pays neither a second discovery nor a second `pty list`. Deliberately advisory: the decision
/// that moves bytes is re-made under the exclusive lock, because a seat can be un-retired between
/// this question and that lock.
pub fn pass_has_work(
    catalog: &Path,
    host: &str,
    specs: &[agent_spec::spec::AgentSpec],
    grace: Duration,
) -> bool {
    pass_has_work_at(catalog, host, specs, grace, crate::message::now_ms())
}

fn pass_has_work_at(
    catalog: &Path,
    host: &str,
    specs: &[agent_spec::spec::AgentSpec],
    grace: Duration,
    now_ms: u64,
) -> bool {
    let retired = retired_identities(specs, host);
    let mut hosts = read_ledger(catalog);
    let (changed, due) = observe_retirements(&mut hosts, host, &retired, grace, now_ms);
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
}
