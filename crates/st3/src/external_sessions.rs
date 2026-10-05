use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::fs::{self, File};
use std::io::{BufRead as _, BufReader, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, LazyLock, Mutex, OnceLock};
use std::time::{Duration, Instant};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result};
#[cfg(not(target_os = "linux"))]
use chrono::NaiveDateTime;
use chrono::{DateTime, Utc};
use kdl::{KdlDocument, KdlEntry, KdlNode};
use rusqlite::{Connection, OpenFlags, params};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use walkdir::WalkDir;

const MAX_DISCOVERED_FILES: usize = 10_000;
pub(crate) const MAX_EXPOSED_HISTORY: usize = 2_000;
const MAX_METADATA_LINES: usize = 64;
const MAX_TIMELINE_LINES: usize = 4_096;
const MAX_TIMELINE_BYTES: u64 = 32 * 1024 * 1024;
// A maximum-size page must remain below the client gateway's one-megabyte response ceiling even
// when every native entry contains a large tool payload.
const MAX_TIMELINE_VALUE_BYTES: usize = 8 * 1024;
const DISCOVERY_CACHE_TTL: Duration = Duration::from_secs(2);
/// How long a saved-history request waits for a background transcript read before it answers
/// with the last complete inventory.
const HISTORY_WAIT: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ExternalDriver {
    Codex,
    Claude,
    Pi,
    Omp,
    OpenCode,
}

impl ExternalDriver {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Pi => "pi",
            Self::Omp => "omp",
            Self::OpenCode => "opencode",
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ExternalProcess {
    pub(crate) pid: u32,
    pub(crate) parent_pid: u32,
    pub(crate) started_at_unix_ms: u128,
    pub(crate) fingerprint: String,
    pub(crate) cwd: Option<PathBuf>,
    pub(crate) command: String,
    pub(crate) exact_session: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct ExternalSession {
    pub(crate) id: String,
    pub(crate) revision: String,
    pub(crate) driver: ExternalDriver,
    pub(crate) native_id: String,
    pub(crate) transcript: PathBuf,
    pub(crate) cwd: Option<PathBuf>,
    pub(crate) title: Option<String>,
    pub(crate) started_at_unix_ms: u128,
    pub(crate) updated_at_unix_ms: u128,
    pub(crate) process: Option<ExternalProcess>,
}

#[derive(Clone, Debug)]
pub(crate) struct UnresolvedProcess {
    pub(crate) id: String,
    pub(crate) revision: String,
    pub(crate) driver: ExternalDriver,
    pub(crate) process: ExternalProcess,
}

#[derive(Clone, Default)]
pub(crate) struct ExternalDiscovery {
    pub(crate) sessions: Vec<ExternalSession>,
    pub(crate) unresolved_processes: Vec<UnresolvedProcess>,
}

pub(crate) enum ExternalConversation {
    Readable(ExternalSession),
    Unavailable(UnresolvedProcess),
}

impl ExternalConversation {
    fn into_readable_session(self) -> Option<ExternalSession> {
        match self {
            Self::Readable(session) => Some(session),
            Self::Unavailable(_) => None,
        }
    }
}

impl ExternalDiscovery {
    pub(crate) fn into_conversation(self, id: &str) -> Option<ExternalConversation> {
        if let Some(session) = self.sessions.into_iter().find(|session| session.id == id) {
            return Some(ExternalConversation::Readable(session));
        }
        self.unresolved_processes
            .into_iter()
            .find(|process| process.id == id)
            .map(ExternalConversation::Unavailable)
    }
}

#[derive(Debug)]
pub(crate) struct AmbiguousSession(pub(crate) String);

impl std::fmt::Display for AmbiguousSession {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            fmt,
            "native session `{}` has multiple matching transcripts with the same timestamp; refusing ambiguous import",
            self.0
        )
    }
}

impl std::error::Error for AmbiguousSession {}

#[derive(Clone, Debug)]
struct SessionMetadata {
    driver: ExternalDriver,
    native_id: String,
    transcript: PathBuf,
    cwd: Option<PathBuf>,
    title: Option<String>,
    started_at_unix_ms: u128,
    updated_at_unix_ms: u128,
    revision: String,
}

pub(crate) fn discover(home: Option<&Path>, include_history: bool) -> Result<ExternalDiscovery> {
    let Some(home) = home else {
        return Ok(ExternalDiscovery::default());
    };
    static CACHE: LazyLock<Mutex<Option<(PathBuf, bool, Instant, ExternalDiscovery)>>> =
        LazyLock::new(|| Mutex::new(None));
    let cached = {
        let cache = CACHE.lock().expect("external session cache mutex poisoned");
        cache
            .as_ref()
            .and_then(|(cached_home, cached_history, created, discovery)| {
                (cached_home == home
                    && *cached_history == include_history
                    && created.elapsed() <= DISCOVERY_CACHE_TTL)
                    .then(|| discovery.clone())
            })
    };
    if let Some(discovery) = cached {
        return Ok(discovery);
    }
    // A historical inventory may be waiting on cold storage. Do not make native-only
    // requests wait behind it by holding the cache mutex while discovering files.
    let discovery = if include_history {
        // Live transcripts are read now; saved history comes from the background inventory,
        // so a cold history tree never holds a request past HISTORY_WAIT.
        let candidates = platform_processes()?;
        let mut files = discover_files(home, false, &candidates)?;
        let live = files
            .iter()
            .map(|item| (item.transcript.clone(), item.native_id.clone()))
            .collect::<BTreeSet<_>>();
        files.extend(
            historical_files(home)?
                .iter()
                .filter(|item| !live.contains(&(item.transcript.clone(), item.native_id.clone())))
                .cloned(),
        );
        assemble_discovery(files, candidates, None, true)?
    } else {
        discover_uncached(home, None, false)?
    };
    *CACHE.lock().expect("external session cache mutex poisoned") = Some((
        home.to_owned(),
        include_history,
        Instant::now(),
        discovery.clone(),
    ));
    Ok(discovery)
}

#[derive(Default)]
struct HistoryInventory {
    files: Option<(Instant, Arc<Vec<SessionMetadata>>)>,
    error: Option<String>,
    refreshing: bool,
    completed: u64,
}

/// Saved transcript metadata per native home, read by a background thread. A daemon has one
/// home; tests use one per temporary directory.
static HISTORY: LazyLock<(Mutex<HashMap<PathBuf, HistoryInventory>>, Condvar)> =
    LazyLock::new(|| (Mutex::new(HashMap::new()), Condvar::new()));

/// Start reading saved transcripts off the request path, unless a read is already running.
pub(crate) fn start_history_inventory(home: Option<&Path>) {
    if let Some(home) = home {
        let mut inventories = HISTORY.0.lock().expect("history inventory mutex poisoned");
        start_history_refresh(inventories.entry(home.to_owned()).or_default(), home);
    }
}

fn start_history_refresh(inventory: &mut HistoryInventory, home: &Path) {
    if inventory.refreshing {
        return;
    }
    inventory.refreshing = true;
    let home = home.to_owned();
    std::thread::spawn(move || {
        let result = discover_files(&home, true, &[]);
        let (lock, ready) = &*HISTORY;
        let mut inventories = lock.lock().expect("history inventory mutex poisoned");
        let inventory = inventories.entry(home).or_default();
        inventory.refreshing = false;
        inventory.completed += 1;
        match result {
            Ok(files) => {
                inventory.files = Some((Instant::now(), Arc::new(files)));
                inventory.error = None;
            }
            Err(error) => inventory.error = Some(format!("{error:#}")),
        }
        ready.notify_all();
    });
}

/// Return saved transcript metadata within HISTORY_WAIT. An inventory older than the cache TTL
/// is refreshed in the background; if that refresh does not finish in time, the last complete
/// inventory is returned. Only before any inventory completes does a request fail, with an
/// error that says to retry.
fn historical_files(home: &Path) -> Result<Arc<Vec<SessionMetadata>>> {
    let (lock, ready) = &*HISTORY;
    let mut inventories = lock.lock().expect("history inventory mutex poisoned");
    let inventory = inventories.entry(home.to_owned()).or_default();
    if let Some((read_at, files)) = &inventory.files
        && read_at.elapsed() <= DISCOVERY_CACHE_TTL
    {
        return Ok(files.clone());
    }
    start_history_refresh(inventory, home);
    let completed = inventory.completed;
    let deadline = Instant::now() + HISTORY_WAIT;
    while inventories[home].completed == completed {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        inventories = ready
            .wait_timeout(inventories, remaining)
            .expect("history inventory mutex poisoned")
            .0;
    }
    let inventory = &inventories[home];
    match (&inventory.files, &inventory.error) {
        (Some((_, files)), _) => Ok(files.clone()),
        (None, Some(error)) => {
            anyhow::bail!("reading saved native session transcripts failed: {error}")
        }
        (None, None) => anyhow::bail!(
            "st is still reading saved native session transcripts in the background; retry shortly"
        ),
    }
}

pub(crate) fn discover_fresh(
    home: Option<&Path>,
    include_history: bool,
) -> Result<ExternalDiscovery> {
    let Some(home) = home else {
        return Ok(ExternalDiscovery::default());
    };
    Ok(discover_uncached(home, None, include_history)?)
}

fn discover_uncached(
    home: &Path,
    strict_id: Option<&str>,
    include_history: bool,
) -> Result<ExternalDiscovery> {
    let candidates = platform_processes()?;
    let files = discover_files(home, include_history, &candidates)?;
    assemble_discovery(files, candidates, strict_id, include_history)
}

fn assemble_discovery(
    files: Vec<SessionMetadata>,
    candidates: Vec<ProcessCandidate>,
    strict_id: Option<&str>,
    include_history: bool,
) -> Result<ExternalDiscovery> {
    let mut metadata = resolve_duplicate_sessions(files, &candidates, strict_id)?;
    metadata.sort_by(|left, right| {
        right
            .updated_at_unix_ms
            .cmp(&left.updated_at_unix_ms)
            .then_with(|| left.native_id.cmp(&right.native_id))
    });
    metadata.truncate(MAX_EXPOSED_HISTORY);

    let roots = root_processes(&candidates)
        .into_iter()
        .filter(|candidate| !candidate.managed_by_st3)
        .collect::<Vec<_>>();
    let candidate_by_pid = candidates
        .iter()
        .map(|candidate| (candidate.process.pid, candidate))
        .collect::<BTreeMap<_, _>>();
    let mut matched_roots = BTreeSet::new();
    let mut sessions = Vec::new();
    for item in metadata {
        let process = candidates.iter().find_map(|candidate| {
            if candidate.driver != item.driver
                || !(candidate.process.command.contains(&item.native_id)
                    || candidate
                        .process
                        .command
                        .contains(item.transcript.to_string_lossy().as_ref()))
            {
                return None;
            }
            let root = process_root(candidate, &candidate_by_pid);
            if root.managed_by_st3 {
                return None;
            }
            matched_roots.insert(root.process.pid);
            let mut process = root.process.clone();
            process.exact_session = true;
            Some(process)
        });
        let id = external_session_id(item.driver, &item.native_id);
        sessions.push(ExternalSession {
            id,
            revision: item.revision,
            driver: item.driver,
            native_id: item.native_id,
            transcript: item.transcript,
            cwd: item.cwd,
            title: item.title,
            started_at_unix_ms: item.started_at_unix_ms,
            updated_at_unix_ms: item.updated_at_unix_ms,
            process,
        });
    }
    let unresolved_processes = roots
        .into_iter()
        .filter(|process| !matched_roots.contains(&process.process.pid))
        .map(|process| unresolved_process(process.driver, process.process))
        .collect();
    Ok(filter_discovery(
        ExternalDiscovery {
            sessions,
            unresolved_processes,
        },
        include_history,
    ))
}

fn resolve_duplicate_sessions(
    metadata: Vec<SessionMetadata>,
    candidates: &[ProcessCandidate],
    strict_id: Option<&str>,
) -> Result<Vec<SessionMetadata>> {
    let mut groups = BTreeMap::<(String, String), Vec<SessionMetadata>>::new();
    for item in metadata {
        groups
            .entry((item.driver.as_str().to_owned(), item.native_id.clone()))
            .or_default()
            .push(item);
    }
    let mut selected = Vec::with_capacity(groups.len());
    for (_, mut group) in groups {
        if group.len() == 1 {
            selected.push(group.pop().expect("one session in group"));
            continue;
        }
        let driver = group[0].driver;
        let native_id = &group[0].native_id;
        let process_cwd = candidates
            .iter()
            .find(|candidate| {
                candidate.driver == driver
                    && !candidate.managed_by_st3
                    && candidate.process.command.contains(native_id)
            })
            .and_then(|candidate| candidate.process.cwd.as_deref())
            .map(|path| fs::canonicalize(path).unwrap_or_else(|_| path.to_owned()));
        let cwd_matches = |item: &SessionMetadata| {
            item.cwd.as_deref().is_some_and(|cwd| {
                process_cwd.as_deref().is_some_and(|process| {
                    fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_owned()) == process
                })
            })
        };
        let best_cwd = group.iter().any(&cwd_matches);
        if best_cwd {
            group.retain(|item| cwd_matches(item));
        }
        group.sort_by(|left, right| {
            right
                .updated_at_unix_ms
                .cmp(&left.updated_at_unix_ms)
                .then_with(|| left.transcript.cmp(&right.transcript))
        });
        let best = group.remove(0);
        if strict_id == Some(external_session_id(driver, &best.native_id).as_str())
            && group
                .first()
                .is_some_and(|other| other.updated_at_unix_ms == best.updated_at_unix_ms)
        {
            return Err(AmbiguousSession(best.native_id).into());
        }
        selected.push(best);
    }
    Ok(selected)
}

fn filter_discovery(mut discovery: ExternalDiscovery, include_history: bool) -> ExternalDiscovery {
    if !include_history {
        discovery
            .sessions
            .retain(|session| session.process.is_some());
    }
    discovery
}

pub(crate) fn find(home: Option<&Path>, id: &str) -> Result<Option<ExternalSession>> {
    Ok(find_conversation(home, id)?.and_then(ExternalConversation::into_readable_session))
}

pub(crate) fn find_conversation(
    home: Option<&Path>,
    id: &str,
) -> Result<Option<ExternalConversation>> {
    if !id.starts_with("session/external-") {
        return Ok(None);
    }
    Ok(discover(home, true)?.into_conversation(id))
}

/// Resolve an already-bound managed transcript without inventorying every native session
/// and process on the machine. The native ID comes from the wrapper binding, not a client.
pub(crate) fn find_bound_transcript(
    home: &Path,
    driver: ExternalDriver,
    native_id: &str,
) -> Result<Option<ExternalSession>> {
    let root = match driver {
        ExternalDriver::Codex => home.join(".codex/sessions"),
        ExternalDriver::Claude => home.join(".claude/projects"),
        _ => return Ok(None),
    };
    let filename = format!("{native_id}.jsonl");
    // Codex prefixes the session ID with the rollout time: `rollout-<time>-<id>.jsonl`.
    let codex_suffix = format!("-{filename}");
    let mut found: Option<SessionMetadata> = None;
    for entry in WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
    {
        let Some(name) = entry.file_name().to_str() else {
            continue;
        };
        if !entry.file_type().is_file()
            || !(name == filename
                || (driver == ExternalDriver::Codex && name.ends_with(&codex_suffix)))
        {
            continue;
        }
        // One unreadable candidate is skipped; it does not end the search.
        let Ok(Some(metadata)) = read_metadata(driver, entry.path()) else {
            continue;
        };
        // Claude names each transcript after its session, so the file name alone identifies
        // it, even when history copied in at the top still carries an earlier session's ID.
        // Other harnesses must also name the session inside the file.
        if driver != ExternalDriver::Claude && metadata.native_id != native_id {
            continue;
        }
        // The same session can leave a file in more than one project directory; the one
        // written most recently is the live one.
        if found
            .as_ref()
            .is_none_or(|current| metadata.updated_at_unix_ms > current.updated_at_unix_ms)
        {
            found = Some(metadata);
        }
    }
    Ok(found.map(|metadata| ExternalSession {
        id: external_session_id(driver, native_id),
        revision: metadata.revision,
        driver,
        native_id: native_id.to_owned(),
        transcript: metadata.transcript,
        cwd: metadata.cwd,
        title: metadata.title,
        started_at_unix_ms: metadata.started_at_unix_ms,
        updated_at_unix_ms: metadata.updated_at_unix_ms,
        process: None,
    }))
}

/// Prove which Claude session a managed seat's live provider is running, from process evidence
/// alone, for when the SessionStart hook never wrote its binding.
///
/// `driver_token` is the `evidence_incarnation` from the seat's own `harness.observed` claim for
/// its current incarnation. The Claude wrapper mints it as `<driver pid>-<unix ms>-<counter>`
/// inside the `st3 driver claude` process, and that process launches Claude as its direct child.
/// Claude records its own session in `~/.claude/sessions/<pid>.json` beside the kernel start time
/// of the process that wrote it. Every link is exact:
///
/// - the driver pid comes from the seat's own claim, never from a client, a scan, or a guess;
/// - that pid must still run `st3 driver claude --subject <this seat>`, and must have started no
///   later than the token was minted, so a reused pid is refused;
/// - the Claude process must be that driver's direct child;
/// - the session file must name that same pid and the same kernel start time, so a file left
///   behind by an earlier process with the same pid is refused;
/// - exactly one session must result.
///
/// Nothing here matches by workspace, recency, or a similar file name. Another seat's Claude has
/// a different driver as its parent and an unmanaged Claude has no driver parent at all, so this
/// cannot select another session's transcript. Any gap yields an explanation, not a guess.
pub(crate) fn claude_session_of_managed_driver(
    home: &Path,
    subject: &str,
    driver_token: &str,
) -> std::result::Result<String, String> {
    // A seat without a wrapper names Claude's own session in its evidence token.
    if let Some(session) = driver_token.strip_prefix(st_drivers::harness_state::WRAPPERLESS_PREFIX)
    {
        return if is_uuid(session) {
            Ok(session.to_owned())
        } else {
            Err(format!(
                "the harness evidence names a malformed Claude session `{session}`"
            ))
        };
    }
    let mut parts = driver_token.splitn(3, '-');
    let (Some(Ok(pid)), Some(Ok(minted_at_ms)), Some(Ok(_))) = (
        parts.next().map(str::parse::<u32>),
        parts.next().map(str::parse::<u128>),
        parts.next().map(str::parse::<u64>),
    ) else {
        return Err(format!(
            "the harness evidence `{driver_token}` does not name a driver process"
        ));
    };
    claude_session_of_driver_process(home, subject, pid, minted_at_ms)
}

#[cfg(target_os = "linux")]
fn claude_session_of_driver_process(
    home: &Path,
    subject: &str,
    pid: u32,
    minted_at_ms: u128,
) -> std::result::Result<String, String> {
    let cmdline = fs::read(format!("/proc/{pid}/cmdline"))
        .map_err(|_| format!("the seat's Claude driver (process {pid}) is no longer running"))?;
    let arguments = cmdline
        .split(|byte| *byte == 0)
        .map(String::from_utf8_lossy)
        .collect::<Vec<_>>();
    // Only the driver's own options count, never the provider argv after `--`.
    let options = arguments
        .iter()
        .position(|argument| argument == "--")
        .map_or(&arguments[..], |end| &arguments[..end]);
    let is_claude_driver = options
        .windows(2)
        .any(|pair| pair[0] == "driver" && pair[1] == "claude");
    let is_this_seat = options
        .windows(2)
        .any(|pair| pair[0] == "--subject" && pair[1] == subject)
        || options
            .iter()
            .any(|argument| argument.strip_prefix("--subject=") == Some(subject));
    if !is_claude_driver || !is_this_seat {
        return Err(format!(
            "process {pid} is not this seat's Claude driver any more"
        ));
    }
    let started_at_ms = linux_process_started_at_ms(pid)
        .ok_or_else(|| format!("the start time of driver process {pid} is unreadable"))?;
    // Boot time is whole seconds, so allow for that rounding and clock-tick granularity.
    if started_at_ms > minted_at_ms.saturating_add(2_000) {
        return Err(format!(
            "process {pid} started after the harness evidence was written, so it is a different process"
        ));
    }
    let sessions = home.join(".claude/sessions");
    let mut found = BTreeSet::new();
    for child in linux_child_processes(pid) {
        let Ok(record) = fs::read(sessions.join(format!("{child}.json"))) else {
            continue;
        };
        let Ok(record) = serde_json::from_slice::<Value>(&record) else {
            continue;
        };
        let recorded_start = match record.get("procStart") {
            Some(Value::String(value)) => value.clone(),
            Some(Value::Number(value)) => value.to_string(),
            _ => continue,
        };
        if record.get("pid").and_then(Value::as_u64) != Some(u64::from(child))
            || linux_process_start_ticks(child).as_deref() != Some(recorded_start.as_str())
        {
            continue;
        }
        if let Some(session) = record.get("sessionId").and_then(Value::as_str)
            && is_uuid(session)
        {
            found.insert(session.to_owned());
        }
    }
    let mut found = found.into_iter();
    match (found.next(), found.next()) {
        (Some(session), None) => Ok(session),
        (None, _) => Err(format!(
            "no live Claude process under driver {pid} has recorded its session in ~/.claude/sessions"
        )),
        (Some(_), Some(_)) => Err(format!(
            "driver {pid} has more than one Claude session, so none is chosen"
        )),
    }
}

#[cfg(not(target_os = "linux"))]
fn claude_session_of_driver_process(
    _home: &Path,
    _subject: &str,
    _pid: u32,
    _minted_at_ms: u128,
) -> std::result::Result<String, String> {
    Err("proving the Claude session from process evidence is supported only on Linux".into())
}

/// The kernel start time of `pid` in clock ticks since boot, as `/proc/<pid>/stat` reports it.
#[cfg(target_os = "linux")]
fn linux_process_start_ticks(pid: u32) -> Option<String> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let end_name = stat.rfind(") ")?;
    stat[end_name + 2..]
        .split_whitespace()
        .nth(19)
        .map(str::to_owned)
}

#[cfg(target_os = "linux")]
fn linux_process_started_at_ms(pid: u32) -> Option<u128> {
    let ticks = linux_process_start_ticks(pid)?.parse::<u128>().ok()?;
    let boot_seconds = fs::read_to_string("/proc/stat")
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("btime "))
        .and_then(|value| value.trim().parse::<u128>().ok())?;
    let per_second = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as u128;
    Some(
        boot_seconds
            .saturating_mul(1_000)
            .saturating_add(ticks.saturating_mul(1_000) / per_second),
    )
}

/// The direct children of `pid`. Every thread's `children` list is read, because the driver
/// launches its provider from a worker thread; a kernel without those lists falls back to a scan
/// of every process's parent.
#[cfg(target_os = "linux")]
fn linux_child_processes(pid: u32) -> BTreeSet<u32> {
    let mut children = BTreeSet::new();
    let mut listed = false;
    if let Ok(tasks) = fs::read_dir(format!("/proc/{pid}/task")) {
        for task in tasks.filter_map(Result::ok) {
            if let Ok(list) = fs::read_to_string(task.path().join("children")) {
                listed = true;
                children.extend(
                    list.split_whitespace()
                        .filter_map(|value| value.parse::<u32>().ok()),
                );
            }
        }
    }
    if listed {
        return children;
    }
    let Ok(processes) = fs::read_dir("/proc") else {
        return children;
    };
    for entry in processes.filter_map(Result::ok) {
        let Some(candidate) = entry
            .file_name()
            .to_str()
            .and_then(|value| value.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(stat) = fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        let parent = stat.rfind(") ").and_then(|end| {
            stat[end + 2..]
                .split_whitespace()
                .nth(1)
                .and_then(|value| value.parse::<u32>().ok())
        });
        if parent == Some(pid) {
            children.insert(candidate);
        }
    }
    children
}

/// Read only the exact pi-family transcript durably named by a native binding or import claim.
pub(crate) fn find_bound_pi_family_transcript(
    driver: ExternalDriver,
    path: &Path,
    native_id: &str,
) -> Result<Option<ExternalSession>> {
    let Some(metadata) = read_metadata(driver, path)? else {
        return Ok(None);
    };
    if metadata.native_id != native_id {
        return Ok(None);
    }
    Ok(Some(ExternalSession {
        id: external_session_id(driver, native_id),
        revision: metadata.revision,
        driver,
        native_id: metadata.native_id,
        transcript: metadata.transcript,
        cwd: metadata.cwd,
        title: metadata.title,
        started_at_unix_ms: metadata.started_at_unix_ms,
        updated_at_unix_ms: metadata.updated_at_unix_ms,
        process: None,
    }))
}

/// Find the newest transcript created by the current managed OMP incarnation.
/// The directory is constructed from the declared seat, never from client input.
pub(crate) fn find_managed_omp_transcript(
    directory: &Path,
    started_after_unix_ms: u128,
) -> Result<Option<ExternalSession>> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    // OMP names sessions with an ISO timestamp prefix. Keep only a bounded set of the newest
    // names while listing, so a large directory costs memory for 64 names rather than failing,
    // and an entry that cannot be inspected is skipped rather than ending the lookup.
    const CANDIDATES: usize = 64;
    let mut files = Vec::new();
    for entry in entries.filter_map(std::result::Result::ok) {
        if entry.file_type().is_ok_and(|kind| kind.is_file())
            && entry.path().extension().is_some_and(|ext| ext == "jsonl")
        {
            files.push(entry.path());
            if files.len() >= CANDIDATES * 4 {
                files.sort_unstable_by(|left, right| right.file_name().cmp(&left.file_name()));
                files.truncate(CANDIDATES);
            }
        }
    }
    // Try a bounded number of newest candidates so a partial header cannot hide a valid
    // predecessor.
    files.sort_unstable_by(|left, right| right.file_name().cmp(&left.file_name()));
    for path in files.into_iter().take(CANDIDATES) {
        let Ok(Some(metadata)) = read_metadata(ExternalDriver::Omp, &path) else {
            continue;
        };
        if metadata.started_at_unix_ms < started_after_unix_ms {
            continue;
        }
        return Ok(Some(ExternalSession {
            id: external_session_id(ExternalDriver::Omp, &metadata.native_id),
            revision: metadata.revision,
            driver: ExternalDriver::Omp,
            native_id: metadata.native_id,
            transcript: metadata.transcript,
            cwd: metadata.cwd,
            title: metadata.title,
            started_at_unix_ms: metadata.started_at_unix_ms,
            updated_at_unix_ms: metadata.updated_at_unix_ms,
            process: None,
        }));
    }
    Ok(None)
}

pub(crate) fn find_fresh(home: Option<&Path>, id: &str) -> Result<Option<ExternalSession>> {
    let Some(home) = home else {
        return Ok(None);
    };
    Ok(discover_uncached(home, Some(id), true)?
        .into_conversation(id)
        .and_then(ExternalConversation::into_readable_session))
}

pub(crate) fn timestamp(unix_ms: u128) -> String {
    let millis = i64::try_from(unix_ms).unwrap_or(i64::MAX);
    DateTime::<Utc>::from_timestamp_millis(millis)
        .unwrap_or(DateTime::<Utc>::UNIX_EPOCH)
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Normalize a native transcript into timeline entries.
///
/// Harnesses change their files between releases, crash mid-write, and occasionally tear a
/// record. The reader is liberal in what it accepts: one unreadable unit (a line that is not
/// UTF-8, is not JSON, or carries a kind this reader does not know) costs only that unit, never
/// the rest of the transcript. What it emits stays conservative: a skipped line or an
/// unrecognized record becomes a clearly-labelled `system` entry, never an entry attributed to
/// the user or the agent. Only opening the file can fail the whole read.
pub(crate) fn normalized_timeline(session: &ExternalSession) -> Result<Vec<Value>> {
    if session.driver == ExternalDriver::OpenCode {
        return normalized_opencode_timeline(session);
    }
    let metadata = fs::metadata(&session.transcript)
        .with_context(|| format!("inspect transcript {}", session.transcript.display()))?;
    let mut file = File::open(&session.transcript)
        .with_context(|| format!("read transcript {}", session.transcript.display()))?;
    let start = metadata.len().saturating_sub(MAX_TIMELINE_BYTES);
    file.seek(SeekFrom::Start(start))?;
    let mut reader = BufReader::new(file);
    let mut read_error = None;
    // Where each line starts in the file: an entry's identity, which must not change as the
    // read window slides along a growing transcript.
    let mut offset = start;
    if start != 0 {
        let mut partial = Vec::new();
        match reader.read_until(b'\n', &mut partial) {
            Ok(count) => offset += count as u64,
            Err(error) => read_error = Some(error),
        }
    }
    // Each line keeps whether it ended with a newline: only the final, unterminated line can be
    // a record the harness is still writing.
    let mut lines = VecDeque::new();
    while read_error.is_none() {
        let mut buffer = Vec::new();
        match reader.read_until(b'\n', &mut buffer) {
            Ok(0) => break,
            Ok(_) => {
                let terminated = buffer.last() == Some(&b'\n');
                if lines.len() == MAX_TIMELINE_LINES {
                    lines.pop_front();
                }
                // A line that is not UTF-8 is read lossily rather than ending the transcript.
                lines.push_back((
                    String::from_utf8_lossy(&buffer).into_owned(),
                    terminated,
                    offset,
                ));
                offset += buffer.len() as u64;
                if !terminated {
                    break;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => read_error = Some(error),
        }
    }
    let mut items = Vec::new();
    if start != 0 || lines.len() == MAX_TIMELINE_LINES {
        items.push(timeline_item(
            0,
            &timestamp(session.updated_at_unix_ms),
            "system",
            "truncation",
            json!({
                "reason": "the native transcript prefix is outside the bounded read window",
                "omitted_from_sequence": 0,
                "omitted_to_sequence": 0
            }),
        ));
    }
    // An entry without its own timestamp takes its predecessor's, so it stays in place when
    // the timeline is merged by time with Small Talk messages.
    let mut last_timestamp = timestamp(session.updated_at_unix_ms);
    let mut next_free_sequence = 0_u64;
    for (line_index, (line, terminated, line_start)) in lines.into_iter().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // A line's entries are numbered from where it starts in the file, so an entry keeps its
        // ID however the read window slides; numbered by position in the window, every entry
        // was renumbered as the transcript grew, and clients saw each one again as new. A line
        // owns sixteen numbers per byte; a record with more parts than that pushes the
        // following lines later instead of colliding with their entry IDs.
        let sequence = line_start
            .saturating_add(1)
            .saturating_mul(16)
            .max(next_free_sequence);
        let first_new = items.len();
        match serde_json::from_str::<Value>(line) {
            Ok(value) => normalize_native_line(
                session.driver,
                &value,
                sequence,
                &last_timestamp,
                &mut items,
            ),
            // The harness is still writing this record; the next read sees it whole.
            Err(_) if !terminated => {}
            Err(error) => {
                let recovered = recover_trailing_record(line);
                push_unreadable_line(
                    &mut items,
                    sequence,
                    &last_timestamp,
                    session.driver,
                    line_index,
                    &error,
                    recovered.is_some(),
                );
                if let Some(value) = recovered {
                    normalize_native_line(
                        session.driver,
                        &value,
                        sequence.saturating_add(1),
                        &last_timestamp,
                        &mut items,
                    );
                }
            }
        }
        if let Some(highest) = items[first_new..]
            .iter()
            .filter_map(|item| item["sequence"].as_u64())
            .max()
        {
            next_free_sequence = highest.saturating_add(1);
        }
        if let Some(stamp) = items[first_new..]
            .last()
            .and_then(|item| item["timestamp"].as_str())
        {
            last_timestamp = stamp.to_owned();
        }
    }
    if let Some(error) = read_error {
        items.push(timeline_item(
            next_free_sequence.max(16),
            &last_timestamp,
            "system",
            "error",
            json!({
                "code": "native-transcript-read-failed",
                "message": format!("st stopped reading the {} transcript early: {error}", session.driver.as_str()),
                "retryable": true,
                "details": {}
            }),
        ));
    }
    items.sort_by_key(|item| item["sequence"].as_u64().unwrap_or(u64::MAX));
    Ok(items)
}

fn normalize_native_line(
    driver: ExternalDriver,
    value: &Value,
    sequence: u64,
    fallback_timestamp: &str,
    items: &mut Vec<Value>,
) {
    match driver {
        ExternalDriver::Codex => normalize_codex(value, sequence, fallback_timestamp, items),
        ExternalDriver::Claude => normalize_claude(value, sequence, fallback_timestamp, items),
        ExternalDriver::Pi | ExternalDriver::Omp => {
            normalize_omp(driver, value, sequence, fallback_timestamp, items)
        }
        // OpenCode history is stored in SQLite and never read line by line.
        ExternalDriver::OpenCode => {}
    }
}

/// Recover a whole record glued behind a torn one.
///
/// A harness killed mid-append leaves a partial record, and its next append lands on the same
/// line: `{"type":"assistant","message":{..."stop_sequence":n{"type":"user",...}`. In valid JSON an
/// object only opens after `:`, `,`, `[`, or at the start, so an object opening after anything
/// else is where the torn record ends. The first such position whose remainder parses as one
/// whole object is the recovered record. The search is bounded.
fn recover_trailing_record(line: &str) -> Option<Value> {
    let bytes = line.as_bytes();
    let mut attempts = 0;
    for (index, _) in line.match_indices('{') {
        if index == 0 {
            continue;
        }
        let previous = bytes[..index]
            .iter()
            .rev()
            .find(|byte| !byte.is_ascii_whitespace());
        if matches!(previous, Some(b':' | b',' | b'[')) {
            continue;
        }
        attempts += 1;
        if attempts > 16 {
            return None;
        }
        if let Ok(value @ Value::Object(_)) = serde_json::from_str::<Value>(&line[index..]) {
            return Some(value);
        }
    }
    None
}

fn push_unreadable_line(
    items: &mut Vec<Value>,
    sequence: u64,
    timestamp: &str,
    driver: ExternalDriver,
    line_index: usize,
    error: &serde_json::Error,
    recovered: bool,
) {
    let message = if recovered {
        format!(
            "st skipped a torn {} transcript record and kept the record written after it",
            driver.as_str()
        )
    } else {
        format!(
            "st skipped a {} transcript line that is not valid JSON",
            driver.as_str()
        )
    };
    items.push(timeline_item(
        sequence,
        timestamp,
        "system",
        "error",
        json!({
            "code": "native-line-unreadable",
            "message": message,
            "retryable": false,
            "details": {
                "line_in_window": line_index.saturating_add(1),
                "parse_error": error.to_string(),
            }
        }),
    ));
}

pub(crate) struct ImportSeat {
    pub(crate) subject: String,
    pub(crate) kdl: String,
}

pub(crate) fn import_seat(session: &ExternalSession) -> Result<ImportSeat> {
    let workspace = session
        .cwd
        .clone()
        .context("the saved session has no recorded workspace")?;
    anyhow::ensure!(
        workspace.is_dir(),
        "the saved session workspace {} does not exist",
        workspace.display()
    );
    // OMP chooses its transcript directory from the real workspace path. A saved
    // transcript may still record a symlink from before a workspace relocation.
    let workspace = if session.driver == ExternalDriver::Omp {
        fs::canonicalize(&workspace)
            .with_context(|| format!("resolve saved session workspace {}", workspace.display()))?
    } else {
        workspace
    };
    let suffix = &digest(&format!(
        "{}:{}",
        session.driver.as_str(),
        session.native_id
    ))[..24];
    let id = format!("import/{}/{suffix}", session.driver.as_str());
    let mut agent = KdlNode::new("agent");
    agent.entries_mut().push(KdlEntry::new(id.clone()));
    let mut agent_body = KdlDocument::new();
    agent_body.nodes_mut().push(string_node(
        "workspace",
        workspace.to_string_lossy().as_ref(),
    ));
    let mut harness = KdlNode::new("harness");
    harness
        .entries_mut()
        .push(KdlEntry::new(session.driver.as_str()));
    let mut harness_body = KdlDocument::new();
    let mut args = KdlNode::new("args");
    match session.driver {
        ExternalDriver::Codex => {
            args.entries_mut().push(KdlEntry::new("resume"));
            args.entries_mut()
                .push(KdlEntry::new(session.native_id.clone()));
        }
        ExternalDriver::Claude => {
            args.entries_mut().push(KdlEntry::new("--resume"));
            args.entries_mut()
                .push(KdlEntry::new(session.native_id.clone()));
        }
        ExternalDriver::Omp => {
            // Managed omp seats use a seat-owned --session-dir. An external
            // session ID cannot be resolved there; the absolute path loads
            // the selected transcript without looking up another copy.
            args.entries_mut().push(KdlEntry::new(format!(
                "--resume={}",
                session.transcript.display()
            )));
        }
        ExternalDriver::Pi => {
            args.entries_mut().push(KdlEntry::new("--session"));
            args.entries_mut().push(KdlEntry::new(
                session.transcript.to_string_lossy().to_string(),
            ));
        }
        ExternalDriver::OpenCode => {
            args.entries_mut().push(KdlEntry::new("--session"));
            args.entries_mut()
                .push(KdlEntry::new(session.native_id.clone()));
        }
    }
    harness_body.nodes_mut().push(args);
    harness.set_children(harness_body);
    agent_body.nodes_mut().push(harness);
    agent_body
        .nodes_mut()
        .push(string_node("restart", "always"));
    agent.set_children(agent_body);

    let mut document = KdlDocument::new();
    let mut version = KdlNode::new("version");
    version.entries_mut().push(KdlEntry::new(2));
    document.nodes_mut().push(version);
    document.nodes_mut().push(agent);
    document.autoformat();
    Ok(ImportSeat {
        subject: format!("agent/{id}"),
        kdl: document.to_string(),
    })
}

pub(crate) fn terminate_exact_process(
    driver: ExternalDriver,
    expected: &ExternalProcess,
) -> Result<()> {
    anyhow::ensure!(
        expected.exact_session,
        "refusing to stop a process without exact native-session evidence"
    );
    let current = platform_processes()?
        .into_iter()
        .find(|candidate| candidate.driver == driver && candidate.process.pid == expected.pid)
        .context("the selected harness process exited before takeover")?;
    anyhow::ensure!(
        current.process.fingerprint == expected.fingerprint,
        "the selected harness process changed before takeover"
    );
    #[cfg(unix)]
    {
        let pid = expected.pid as i32;
        let pgid = unsafe { libc::getpgid(pid) };
        let target = if pgid == pid { -pid } else { pid };
        if unsafe { libc::kill(target, libc::SIGTERM) } != 0 {
            let error = std::io::Error::last_os_error();
            anyhow::ensure!(
                error.raw_os_error() == Some(libc::ESRCH),
                "stop imported harness process {pid}: {error}"
            );
        }
        for _ in 0..100 {
            if !process_is_live(pid) {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        if unsafe { libc::kill(target, libc::SIGKILL) } != 0 {
            let error = std::io::Error::last_os_error();
            anyhow::ensure!(
                error.raw_os_error() == Some(libc::ESRCH),
                "kill imported harness process {pid}: {error}"
            );
        }
        Ok(())
    }
    #[cfg(not(unix))]
    anyhow::bail!("session takeover is supported only on Unix hosts")
}

#[cfg(target_os = "linux")]
fn process_is_live(pid: i32) -> bool {
    let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    let Some(end_name) = stat.rfind(") ") else {
        return false;
    };
    !matches!(stat[end_name + 2..].chars().next(), Some('Z' | 'X'))
}

#[cfg(all(unix, not(target_os = "linux")))]
fn process_is_live(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

fn string_node(name: &str, value: &str) -> KdlNode {
    let mut node = KdlNode::new(name);
    node.entries_mut().push(KdlEntry::new(value));
    node
}

fn discover_files(
    home: &Path,
    include_history: bool,
    candidates: &[ProcessCandidate],
) -> Result<Vec<SessionMetadata>> {
    let roots = [
        (ExternalDriver::Codex, home.join(".codex/sessions")),
        (ExternalDriver::Claude, home.join(".claude/projects")),
        (ExternalDriver::Pi, home.join(".pi/agent/sessions")),
        (ExternalDriver::Omp, home.join(".oh-omp/agent/sessions")),
        (ExternalDriver::Omp, home.join(".omp/agent/sessions")),
    ];
    // A native-only listing needs only transcripts named by live harness commands. Walking
    // historical Codex/OMP trees reads cold files on every first request and can outlast the
    // client deadline even when no historical sessions are requested. Processes with no exact
    // transcript path remain visible as unresolved processes.
    if !include_history {
        let mut found = if candidates.iter().any(|candidate| {
            candidate.driver == ExternalDriver::OpenCode && !candidate.managed_by_st3
        }) {
            discover_opencode_sessions(home).unwrap_or_default()
        } else {
            Vec::new()
        };
        let mut seen = BTreeSet::new();
        for candidate in candidates
            .iter()
            .filter(|candidate| !candidate.managed_by_st3)
        {
            for token in candidate.process.command.split_whitespace() {
                let token = token.trim_matches(['"', '\'']);
                let token = if token.starts_with('-') {
                    token.split_once('=').map_or(token, |(_, path)| path)
                } else {
                    token
                };
                let path = Path::new(token);
                if path
                    .extension()
                    .is_none_or(|extension| extension != "jsonl")
                    || !path.is_absolute()
                {
                    continue;
                }
                let in_session_root = roots.iter().any(|(driver, root)| {
                    if *driver != candidate.driver {
                        return false;
                    }
                    let Ok(canonical_root) = fs::canonicalize(root) else {
                        return false;
                    };
                    if !path.starts_with(root) && !path.starts_with(&canonical_root) {
                        return false;
                    }
                    let Ok(canonical_path) = fs::canonicalize(path) else {
                        return false;
                    };
                    canonical_path
                        .strip_prefix(canonical_root)
                        .is_ok_and(|relative| {
                            let depth = relative.components().count();
                            depth >= 2 && (*driver != ExternalDriver::Omp || depth == 2)
                        })
                });
                if in_session_root
                    && seen.insert(path.to_owned())
                    && found.len() < MAX_DISCOVERED_FILES
                    && let Ok(Some(metadata)) = read_metadata(candidate.driver, path)
                {
                    found.push(metadata);
                }
            }
        }
        return Ok(found);
    }
    // One session source or file that cannot be read, such as a locked database, a transcript
    // deleted while the walk ran, or a line that is not UTF-8, is skipped. The sessions that can be
    // read are still listed.
    let mut found = discover_opencode_sessions(home).unwrap_or_default();
    for (driver, root) in roots {
        if !root.is_dir() {
            continue;
        }
        // OMP stores tool-call JSONL files below each session's attachment directory.
        // Only the files directly below a project directory are resumable transcripts.
        let max_depth = if driver == ExternalDriver::Omp {
            2
        } else {
            usize::MAX
        };
        for entry in WalkDir::new(root)
            .max_depth(max_depth)
            .follow_links(false)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_file())
            .filter(|entry| {
                entry
                    .path()
                    .extension()
                    .is_some_and(|value| value == "jsonl")
            })
        {
            if found.len() >= MAX_DISCOVERED_FILES {
                break;
            }
            if let Ok(Some(metadata)) = read_metadata(driver, entry.path()) {
                found.push(metadata);
            }
        }
    }
    Ok(found)
}

fn open_opencode_database(path: &Path) -> Result<Connection> {
    Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("open OpenCode session database {}", path.display()))
}

fn discover_opencode_sessions(home: &Path) -> Result<Vec<SessionMetadata>> {
    let database = home.join(".local/share/opencode/opencode.db");
    if !database.is_file() {
        return Ok(Vec::new());
    }
    let file_metadata = fs::metadata(&database)?;
    let database_updated_at = system_time_ms(file_metadata.modified().unwrap_or(UNIX_EPOCH));
    let connection = open_opencode_database(&database)?;
    let mut statement = connection.prepare(
        "SELECT s.id, s.directory, s.title, s.time_created, s.time_updated, \
                COALESCE(MAX(m.time_updated), 0), COUNT(m.id) \
         FROM session s LEFT JOIN message m ON m.session_id = s.id \
         GROUP BY s.id \
         ORDER BY s.time_updated DESC LIMIT ?1",
    )?;
    let rows = statement.query_map(params![MAX_EXPOSED_HISTORY as i64], |row| {
        let native_id: String = row.get(0)?;
        let directory: Option<String> = row.get(1)?;
        let title: Option<String> = row.get(2)?;
        let created: i64 = row.get(3)?;
        let updated: i64 = row.get(4)?;
        let message_updated: i64 = row.get(5)?;
        let message_count: i64 = row.get(6)?;
        Ok((
            native_id,
            directory,
            title,
            created,
            updated,
            message_updated,
            message_count,
        ))
    })?;
    let mut found = Vec::new();
    for row in rows {
        let (native_id, directory, title, created, updated, message_updated, message_count) = row?;
        if native_id.trim().is_empty() {
            continue;
        }
        let started_at_unix_ms = created.max(0) as u128;
        let updated_at_unix_ms = updated.max(message_updated).max(0) as u128;
        let revision = digest(&format!(
            "opencode:{}:{}:{}:{}:{}:{}",
            database.display(),
            native_id,
            updated_at_unix_ms,
            message_count,
            file_metadata.len(),
            database_updated_at,
        ));
        found.push(SessionMetadata {
            driver: ExternalDriver::OpenCode,
            native_id,
            transcript: database.clone(),
            cwd: directory
                .filter(|value| !value.trim().is_empty())
                .map(PathBuf::from),
            title: title.filter(|value| !value.trim().is_empty()),
            started_at_unix_ms,
            updated_at_unix_ms,
            revision,
        });
    }
    Ok(found)
}

fn read_metadata(driver: ExternalDriver, path: &Path) -> Result<Option<SessionMetadata>> {
    let file_metadata = fs::metadata(path)?;
    let modified = file_metadata.modified().unwrap_or(UNIX_EPOCH);
    let length = file_metadata.len();
    // Discovery revisits the same transcript files on every client refresh. Their
    // opening metadata is immutable for almost all of those visits, while parsing
    // hundreds of JSONL headers repeatedly dominates the idle daemon's CPU.
    static METADATA_CACHE: OnceLock<
        Mutex<HashMap<PathBuf, (ExternalDriver, u64, SystemTime, Option<SessionMetadata>)>>,
    > = OnceLock::new();
    let cache = METADATA_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some((cached_driver, cached_length, cached_modified, cached)) = cache
        .lock()
        .expect("external session metadata cache mutex poisoned")
        .get(path)
        && *cached_driver == driver
        && *cached_length == length
        && *cached_modified == modified
    {
        return Ok(cached.clone());
    }
    let parsed = parse_metadata(driver, path, &file_metadata)?;
    let mut cache = cache
        .lock()
        .expect("external session metadata cache mutex poisoned");
    if cache.len() >= MAX_DISCOVERED_FILES && !cache.contains_key(path) {
        cache.clear();
    }
    cache.insert(path.to_owned(), (driver, length, modified, parsed.clone()));
    Ok(parsed)
}

fn parse_metadata(
    driver: ExternalDriver,
    path: &Path,
    file_metadata: &fs::Metadata,
) -> Result<Option<SessionMetadata>> {
    let updated_at_unix_ms = system_time_ms(file_metadata.modified().unwrap_or(UNIX_EPOCH));
    let file = File::open(path)?;
    let mut native_id = None;
    let mut cwd = None;
    let mut title = None;
    let mut started_at_unix_ms = None;
    let mut reader = BufReader::new(file);
    for _ in 0..MAX_METADATA_LINES {
        // A header line that is not UTF-8 or not JSON is skipped, not fatal to the session.
        let mut line = Vec::new();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let Ok(value) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        match driver {
            ExternalDriver::Codex if value["type"] == "session_meta" => {
                native_id = value
                    .pointer("/payload/id")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                cwd = value
                    .pointer("/payload/cwd")
                    .and_then(Value::as_str)
                    .map(PathBuf::from);
                started_at_unix_ms = parse_timestamp(value.get("timestamp"));
                title = value
                    .pointer("/payload/source")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                break;
            }
            ExternalDriver::Claude => {
                native_id = native_id.or_else(|| {
                    value
                        .get("sessionId")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                });
                cwd = cwd.or_else(|| value.get("cwd").and_then(Value::as_str).map(PathBuf::from));
                started_at_unix_ms =
                    started_at_unix_ms.or_else(|| parse_timestamp(value.get("timestamp")));
                title =
                    title.or_else(|| value.get("slug").and_then(Value::as_str).map(str::to_owned));
            }
            ExternalDriver::Pi | ExternalDriver::Omp if value["type"] == "session" => {
                native_id = value.get("id").and_then(Value::as_str).map(str::to_owned);
                cwd = value.get("cwd").and_then(Value::as_str).map(PathBuf::from);
                started_at_unix_ms = parse_timestamp(value.get("timestamp"));
                title = value
                    .get("title")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                break;
            }
            ExternalDriver::OpenCode => unreachable!("OpenCode metadata is stored in SQLite"),
            _ => {}
        }
    }
    if native_id.is_none() {
        native_id = native_id_from_file_name(driver, path);
    }
    let Some(native_id) = native_id.filter(|value| !value.trim().is_empty()) else {
        return Ok(None);
    };
    let started_at_unix_ms = started_at_unix_ms.unwrap_or(updated_at_unix_ms);
    let revision = digest(&format!(
        "{}:{}:{}:{}",
        driver.as_str(),
        path.display(),
        file_metadata.len(),
        updated_at_unix_ms
    ));
    Ok(Some(SessionMetadata {
        driver,
        native_id,
        transcript: path.to_owned(),
        cwd,
        title,
        started_at_unix_ms,
        updated_at_unix_ms,
        revision,
    }))
}

/// The session ID a harness encodes in its own transcript file name, used when the header that
/// normally names it is missing or unreadable. Claude names the file `<session>.jsonl`; Codex
/// `rollout-<time>-<session>.jsonl`; Pi `<time>_<session>.jsonl`. Only a UUID-shaped suffix counts
/// for the latter, so an arbitrary file name never becomes a session ID.
fn native_id_from_file_name(driver: ExternalDriver, path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    match driver {
        ExternalDriver::Claude => Some(stem.to_owned()),
        ExternalDriver::Codex | ExternalDriver::Pi | ExternalDriver::Omp => {
            let suffix = stem.get(stem.len().checked_sub(36)?..)?;
            let separated =
                stem.len() == 36 || matches!(stem.as_bytes()[stem.len() - 37], b'-' | b'_');
            (separated && is_uuid(suffix)).then(|| suffix.to_owned())
        }
        ExternalDriver::OpenCode => None,
    }
}

fn is_uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}

#[derive(Clone)]
struct ProcessCandidate {
    driver: ExternalDriver,
    process: ExternalProcess,
    managed_by_st3: bool,
}

fn root_processes(candidates: &[ProcessCandidate]) -> Vec<ProcessCandidate> {
    let parents = candidates
        .iter()
        .map(|candidate| (candidate.process.pid, candidate.driver))
        .collect::<BTreeMap<_, _>>();
    candidates
        .iter()
        .filter(|candidate| parents.get(&candidate.process.parent_pid) != Some(&candidate.driver))
        .cloned()
        .collect()
}

fn process_root<'a>(
    candidate: &'a ProcessCandidate,
    candidates: &BTreeMap<u32, &'a ProcessCandidate>,
) -> &'a ProcessCandidate {
    let mut current = candidate;
    while let Some(parent) = candidates.get(&current.process.parent_pid)
        && parent.driver == current.driver
    {
        current = parent;
    }
    current
}

fn unresolved_process(driver: ExternalDriver, process: ExternalProcess) -> UnresolvedProcess {
    let fingerprint = process.fingerprint.clone();
    let id = format!(
        "session/external-process-{}",
        &digest(&format!("{}:{fingerprint}", driver.as_str()))[..24]
    );
    UnresolvedProcess {
        id,
        revision: digest(&fingerprint),
        driver,
        process,
    }
}

fn platform_processes() -> Result<Vec<ProcessCandidate>> {
    #[cfg(target_os = "linux")]
    {
        linux_processes()
    }
    #[cfg(not(target_os = "linux"))]
    {
        ps_processes()
    }
}

#[cfg(target_os = "linux")]
fn linux_processes() -> Result<Vec<ProcessCandidate>> {
    let boot_seconds = fs::read_to_string("/proc/stat")?
        .lines()
        .find_map(|line| line.strip_prefix("btime "))
        .and_then(|value| value.parse::<u128>().ok())
        .unwrap_or_default();
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as u128;
    let mut found = Vec::new();
    for entry in fs::read_dir("/proc")? {
        let entry = entry?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|value| value.parse::<u32>().ok())
        else {
            continue;
        };
        let bytes = match fs::read(entry.path().join("cmdline")) {
            Ok(bytes) if !bytes.is_empty() => bytes,
            _ => continue,
        };
        let command = String::from_utf8_lossy(&bytes).replace('\0', " ");
        let Some(driver) = driver_for_command(&command) else {
            continue;
        };
        let stat = fs::read_to_string(entry.path().join("stat")).unwrap_or_default();
        let Some(end_name) = stat.rfind(") ") else {
            continue;
        };
        let fields = stat[end_name + 2..].split_whitespace().collect::<Vec<_>>();
        let parent_pid = fields
            .get(1)
            .and_then(|value| value.parse().ok())
            .unwrap_or_default();
        let start_ticks = fields
            .get(19)
            .and_then(|value| value.parse::<u128>().ok())
            .unwrap_or_default();
        let started_at_unix_ms = boot_seconds
            .saturating_mul(1_000)
            .saturating_add(start_ticks.saturating_mul(1_000) / ticks);
        let cwd = fs::read_link(entry.path().join("cwd")).ok();
        let fingerprint = process_fingerprint(pid, started_at_unix_ms, &command);
        found.push(ProcessCandidate {
            driver,
            managed_by_st3: is_st3_driver(&command),
            process: ExternalProcess {
                pid,
                parent_pid,
                started_at_unix_ms,
                fingerprint,
                cwd,
                command,
                exact_session: false,
            },
        });
    }
    Ok(found)
}

#[cfg(not(target_os = "linux"))]
fn ps_processes() -> Result<Vec<ProcessCandidate>> {
    let output = crate::environment::command("ps")?
        .args(["-axo", "pid=,ppid=,lstart=,command="])
        .output()
        .context("list local harness processes")?;
    anyhow::ensure!(
        output.status.success(),
        "ps failed while listing harness processes"
    );
    let mut found = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let fields = line.split_whitespace().collect::<Vec<_>>();
        if fields.len() < 8 {
            continue;
        }
        let Ok(pid) = fields[0].parse::<u32>() else {
            continue;
        };
        let parent_pid = fields[1].parse::<u32>().unwrap_or_default();
        let date = fields[2..7].join(" ");
        let started_at_unix_ms = NaiveDateTime::parse_from_str(&date, "%a %b %e %H:%M:%S %Y")
            .map(|value| value.and_utc().timestamp_millis().max(0) as u128)
            .unwrap_or_default();
        let command = fields[7..].join(" ");
        let Some(driver) = driver_for_command(&command) else {
            continue;
        };
        let cwd = process_cwd_from_lsof(pid);
        let fingerprint = process_fingerprint(pid, started_at_unix_ms, &command);
        found.push(ProcessCandidate {
            driver,
            managed_by_st3: is_st3_driver(&command),
            process: ExternalProcess {
                pid,
                parent_pid,
                started_at_unix_ms,
                fingerprint,
                cwd,
                command,
                exact_session: false,
            },
        });
    }
    Ok(found)
}

#[cfg(not(target_os = "linux"))]
fn process_cwd_from_lsof(pid: u32) -> Option<PathBuf> {
    let output = crate::environment::command("lsof")
        .ok()?
        .args(["-a", "-p", &pid.to_string(), "-d", "cwd", "-Fn"])
        .output()
        .ok()?;
    output.status.success().then_some(())?;
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.strip_prefix('n'))
        .map(PathBuf::from)
}

fn driver_for_command(command: &str) -> Option<ExternalDriver> {
    let tokens = command.split_whitespace().collect::<Vec<_>>();
    if is_omp_worker_command(&tokens) {
        return None;
    }
    for driver in [
        ExternalDriver::Codex,
        ExternalDriver::Claude,
        ExternalDriver::Pi,
        ExternalDriver::Omp,
        ExternalDriver::OpenCode,
    ] {
        let name = driver.as_str();
        if tokens.first().and_then(|value| command_basename(value)) == Some(name)
            || matches!(
                tokens.first().and_then(|value| command_basename(value)),
                Some("node" | "bun")
            ) && tokens.get(1).and_then(|value| command_basename(value)) == Some(name)
            || matches!(
                tokens.first().and_then(|value| command_basename(value)),
                Some("st2" | "st3")
            ) && tokens.windows(2).any(|pair| pair == ["driver", name])
        {
            return Some(driver);
        }
    }
    None
}

fn is_omp_worker_command(tokens: &[&str]) -> bool {
    let entry = match tokens.first().and_then(|value| command_basename(value)) {
        Some("omp") => 0,
        Some("node" | "bun")
            if tokens.get(1).and_then(|value| command_basename(value)) == Some("omp") =>
        {
            1
        }
        _ => return false,
    };
    // Internal modes such as __omp_worker_daemon_broker, __omp_worker_js_eval_process,
    // __omp_worker_lsp_mux and __omp_worker_text_predict are helpers, not harness sessions.
    tokens
        .get(entry + 1)
        .is_some_and(|argument| argument.starts_with("__omp_worker_"))
}

fn command_basename(value: &str) -> Option<&str> {
    Path::new(value).file_name().and_then(|name| name.to_str())
}

fn is_st3_driver(command: &str) -> bool {
    let tokens = command.split_whitespace().collect::<Vec<_>>();
    tokens.first().and_then(|value| command_basename(value)) == Some("st3")
        && tokens
            .windows(2)
            .any(|pair| pair.first() == Some(&"driver"))
}

fn process_fingerprint(pid: u32, started_at_unix_ms: u128, command: &str) -> String {
    format!("{pid}:{started_at_unix_ms}:{}", digest(command))
}

fn external_session_id(driver: ExternalDriver, native_id: &str) -> String {
    format!(
        "session/external-{}",
        &digest(&format!("{}:{native_id}", driver.as_str()))[..24]
    )
}

fn normalized_opencode_timeline(session: &ExternalSession) -> Result<Vec<Value>> {
    let connection = open_opencode_database(&session.transcript)?;
    let mut message_statement = connection.prepare(
        "SELECT id, time_created, data FROM (\
             SELECT id, time_created, data FROM message \
             WHERE session_id = ?1 ORDER BY time_created DESC, id DESC LIMIT ?2\
         ) ORDER BY time_created, id",
    )?;
    // Rows are decoded one column at a time, so one row with an unexpected column type costs
    // only itself. The count of rows that could not be decoded is shown, not hidden.
    let mut skipped_rows = 0_usize;
    let mut messages = Vec::new();
    for row in message_statement.query_map(
        params![session.native_id, MAX_TIMELINE_LINES as i64 + 1],
        |row| {
            Ok((
                row.get::<_, Option<String>>(0).ok().flatten(),
                row.get::<_, Option<i64>>(1).ok().flatten(),
                row.get::<_, Option<String>>(2).ok().flatten(),
            ))
        },
    )? {
        match row {
            Ok((Some(id), created, encoded)) => messages.push((id, created, encoded)),
            _ => skipped_rows += 1,
        }
    }
    let mut truncated = messages.len() > MAX_TIMELINE_LINES;
    if truncated {
        messages.remove(0);
    }
    // A database from an OpenCode release without the part table still shows its messages.
    let mut part_statement = connection
        .prepare(
            "SELECT data FROM part WHERE session_id = ?1 AND message_id = ?2 \
             ORDER BY time_created, id",
        )
        .ok();
    let parts_unavailable = part_statement.is_none();
    let mut items = VecDeque::new();
    // Include the surrounding JSON array delimiters so this remains an exact bound on the
    // serialized timeline, not just on its native payloads.
    let mut serialized_bytes = 2_usize;
    let mut sequence = 1_u64;
    let mut last_created = session.started_at_unix_ms;
    for (message_id, created, encoded) in messages {
        let message = encoded
            .and_then(|encoded| serde_json::from_str::<Value>(&encoded).ok())
            .unwrap_or(Value::Null);
        let role = normalized_role(message.get("role").and_then(Value::as_str));
        if let Some(created) = created {
            last_created = created.max(0) as u128;
        }
        let at = timestamp(last_created);
        let mut additions = Vec::with_capacity(2);
        push_message(
            &mut additions,
            next_opencode_sequence(&mut sequence)?,
            &at,
            role,
            &message_id,
        );
        extend_bounded_opencode_timeline(
            &mut items,
            &mut serialized_bytes,
            &mut truncated,
            additions,
        );
        let Some(part_statement) = part_statement.as_mut() else {
            continue;
        };
        let parts = match part_statement.query_map(params![session.native_id, message_id], |row| {
            row.get::<_, Option<String>>(0)
        }) {
            Ok(parts) => parts.collect::<Vec<_>>(),
            Err(_) => {
                skipped_rows += 1;
                continue;
            }
        };
        for encoded_part in parts {
            let Some(part) = encoded_part
                .ok()
                .flatten()
                .and_then(|encoded| serde_json::from_str::<Value>(&encoded).ok())
            else {
                skipped_rows += 1;
                continue;
            };
            let mut additions = Vec::with_capacity(2);
            match part.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(text) = part.get("text").and_then(Value::as_str) {
                        push_content(
                            &mut additions,
                            next_opencode_sequence(&mut sequence)?,
                            &at,
                            role,
                            text,
                        );
                    }
                }
                Some("tool") => {
                    let call_id = part
                        .get("callID")
                        .or_else(|| part.get("callId"))
                        .and_then(Value::as_str)
                        .unwrap_or("native-call");
                    let name = part.get("tool").and_then(Value::as_str).unwrap_or("tool");
                    let state = part.get("state").unwrap_or(&Value::Null);
                    push_tool_call(
                        &mut additions,
                        next_opencode_sequence(&mut sequence)?,
                        &at,
                        call_id,
                        name,
                        state.get("input").cloned().unwrap_or_else(|| json!({})),
                    );
                    let status = state.get("status").and_then(Value::as_str);
                    if matches!(status, Some("completed" | "error")) {
                        push_tool_result_with_status(
                            &mut additions,
                            next_opencode_sequence(&mut sequence)?,
                            &at,
                            call_id,
                            state
                                .get("output")
                                .or_else(|| state.get("error"))
                                .cloned()
                                .unwrap_or(Value::Null),
                            status == Some("error"),
                        );
                    }
                }
                Some("file") => {
                    let name = part
                        .get("filename")
                        .or_else(|| part.get("url"))
                        .and_then(Value::as_str)
                        .unwrap_or("attachment");
                    push_content(
                        &mut additions,
                        next_opencode_sequence(&mut sequence)?,
                        &at,
                        role,
                        &format!("[file: {name}]"),
                    );
                }
                Some(kind) if OPENCODE_HIDDEN_PARTS.contains(&kind) => {}
                kind => push_unrecognized(
                    &mut additions,
                    next_opencode_sequence(&mut sequence)?,
                    &at,
                    "opencode",
                    "part",
                    kind,
                    &part,
                ),
            }
            extend_bounded_opencode_timeline(
                &mut items,
                &mut serialized_bytes,
                &mut truncated,
                additions,
            );
        }
    }
    if skipped_rows > 0 || parts_unavailable {
        let message = if parts_unavailable {
            "st could not read OpenCode's part table, so message contents are missing".to_owned()
        } else {
            format!("st skipped {skipped_rows} OpenCode rows it could not decode")
        };
        let notice = timeline_item(
            next_opencode_sequence(&mut sequence)?,
            &timestamp(session.updated_at_unix_ms),
            "system",
            "error",
            json!({
                "code": "native-rows-unreadable",
                "message": message,
                "retryable": false,
                "details": {"skipped_rows": skipped_rows}
            }),
        );
        extend_bounded_opencode_timeline(
            &mut items,
            &mut serialized_bytes,
            &mut truncated,
            vec![notice],
        );
    }
    if truncated {
        prepend_opencode_truncation(
            &mut items,
            &mut serialized_bytes,
            timeline_item(
                0,
                &timestamp(session.updated_at_unix_ms),
                "system",
                "truncation",
                json!({
                    "reason": "the native OpenCode history prefix is outside the bounded read window",
                    "omitted_from_sequence": 0,
                    "omitted_to_sequence": 0
                }),
            ),
        );
    }
    Ok(items.into_iter().map(|(item, _)| item).collect())
}

fn next_opencode_sequence(sequence: &mut u64) -> Result<u64> {
    let current = *sequence;
    *sequence = (*sequence)
        .checked_add(1)
        .context("OpenCode timeline exceeds sequence capacity")?;
    Ok(current)
}

fn extend_bounded_opencode_timeline(
    items: &mut VecDeque<(Value, usize)>,
    serialized_bytes: &mut usize,
    truncated: &mut bool,
    additions: Vec<Value>,
) {
    let byte_limit = usize::try_from(MAX_TIMELINE_BYTES).unwrap_or(usize::MAX);
    for item in additions {
        let item_bytes = serde_json::to_vec(&item).map_or(byte_limit, |encoded| encoded.len());
        let mut additional_bytes = item_bytes + usize::from(!items.is_empty());
        while items.len() >= MAX_TIMELINE_LINES
            || serialized_bytes.saturating_add(additional_bytes) > byte_limit
        {
            if !pop_opencode_timeline_front(items, serialized_bytes) {
                break;
            }
            *truncated = true;
            additional_bytes = item_bytes + usize::from(!items.is_empty());
        }
        if serialized_bytes.saturating_add(additional_bytes) > byte_limit {
            *truncated = true;
            continue;
        }
        *serialized_bytes = serialized_bytes.saturating_add(additional_bytes);
        items.push_back((item, item_bytes));
    }
}

fn pop_opencode_timeline_front(
    items: &mut VecDeque<(Value, usize)>,
    serialized_bytes: &mut usize,
) -> bool {
    let Some((_, removed_bytes)) = items.pop_front() else {
        return false;
    };
    let removed_comma = usize::from(!items.is_empty());
    *serialized_bytes = serialized_bytes.saturating_sub(removed_bytes + removed_comma);
    true
}

fn prepend_opencode_truncation(
    items: &mut VecDeque<(Value, usize)>,
    serialized_bytes: &mut usize,
    mut truncation: Value,
) {
    let byte_limit = usize::try_from(MAX_TIMELINE_BYTES).unwrap_or(usize::MAX);
    while items.len() >= MAX_TIMELINE_LINES {
        if !pop_opencode_timeline_front(items, serialized_bytes) {
            break;
        }
    }
    loop {
        truncation["body"]["omitted_to_sequence"] = items
            .front()
            .and_then(|(item, _)| item["sequence"].as_u64())
            .unwrap_or(0)
            .saturating_sub(1)
            .into();
        let truncation_bytes =
            serde_json::to_vec(&truncation).map_or(byte_limit, |encoded| encoded.len());
        let additional_bytes = truncation_bytes + usize::from(!items.is_empty());
        if serialized_bytes.saturating_add(additional_bytes) <= byte_limit {
            *serialized_bytes = serialized_bytes.saturating_add(additional_bytes);
            items.push_front((truncation, truncation_bytes));
            break;
        }
        if !pop_opencode_timeline_front(items, serialized_bytes) {
            break;
        }
    }
}

/// Codex records that are known and are not conversation: session headers, the event stream
/// that mirrors response items, turn settings, and accounting.
const CODEX_BOOKKEEPING: &[&str] = &[
    "session_meta",
    "event_msg",
    "turn_context",
    "compacted",
    "token_usage_record",
    "world_state",
];
/// Codex response items that are known and deliberately not shown (private reasoning and
/// workspace snapshots).
const CODEX_HIDDEN_ITEMS: &[&str] = &["reasoning", "ghost_snapshot"];
/// Claude records that are known and are not conversation: UI and session bookkeeping.
const CLAUDE_BOOKKEEPING: &[&str] = &[
    "file-history-snapshot",
    "file-history-delta",
    "queue-operation",
    "permission-mode",
    "mode",
    "atis-latch",
    "last-prompt",
    "ai-title",
    "custom-title",
    "summary",
    "cost-state",
    "progress",
    "agent-name",
    "tag",
    "pr-link",
    "bridge-session",
    "fork-context-ref",
];
/// Content blocks that are known and deliberately not shown: the model's private reasoning.
const HIDDEN_REASONING_BLOCKS: &[&str] = &["thinking", "redacted_thinking"];
/// Pi and OMP records that are known and are not conversation.
const OMP_BOOKKEEPING: &[&str] = &[
    "session",
    "model_change",
    "thinking_level_change",
    "label",
    "session_info",
    "custom",
    "credential_pin",
    "title",
];
/// OpenCode parts that are known and deliberately not shown.
const OPENCODE_HIDDEN_PARTS: &[&str] = &[
    "reasoning",
    "step-start",
    "step-finish",
    "snapshot",
    "patch",
    "agent",
    "retry",
    "compaction",
];
/// The most JSON an unrecognized record contributes to its generic entry.
const MAX_UNRECOGNIZED_BYTES: usize = 512;

/// A native timestamp as the timeline carries it. RFC 3339 text is kept as written; a number is
/// Unix milliseconds (or seconds, when too small to be milliseconds). Anything else is absent,
/// so the caller's fallback applies instead of an unparseable timestamp reaching a client.
fn native_timestamp(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(text) => DateTime::parse_from_rfc3339(text)
            .ok()
            .map(|_| text.clone()),
        Value::Number(number) => {
            let value = number.as_u64()?;
            let millis = if value < 100_000_000_000 {
                value.saturating_mul(1_000)
            } else {
                value
            };
            Some(timestamp(u128::from(millis)))
        }
        _ => None,
    }
}

/// Show a record this reader does not understand as a generic, clearly-labelled system entry
/// with a bounded excerpt, instead of dropping it. The role is always `system`: an unknown
/// record is never attributed to the user or the agent.
fn push_unrecognized(
    items: &mut Vec<Value>,
    sequence: u64,
    timestamp: &str,
    driver: &str,
    what: &str,
    kind: Option<&str>,
    value: &Value,
) {
    if driver == "omp" && omp_has_image_payload(value) {
        push_omp_image_unavailable(items, sequence, timestamp, value);
        return;
    }
    let label = match kind {
        Some(kind) => format!("[unrecognized {driver} {what} `{kind}`]"),
        None => format!("[unrecognized {driver} {what} without a type]"),
    };
    let encoded = serde_json::to_string(value).unwrap_or_default();
    let excerpt = truncate_at_char_boundary(&encoded, MAX_UNRECOGNIZED_BYTES);
    let ellipsis = if excerpt.len() < encoded.len() {
        "…"
    } else {
        ""
    };
    items.push(timeline_item(
        sequence,
        timestamp,
        "system",
        "content",
        json!({"media_type":"text/plain", "text":format!("{label}\n{excerpt}{ellipsis}")}),
    ));
}

fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn normalize_codex(value: &Value, sequence: u64, fallback_timestamp: &str, items: &mut Vec<Value>) {
    let timestamp =
        native_timestamp(value.get("timestamp")).unwrap_or_else(|| fallback_timestamp.to_owned());
    match value.get("type").and_then(Value::as_str) {
        Some("response_item") => {}
        Some(kind) if CODEX_BOOKKEEPING.contains(&kind) => return,
        kind => {
            push_unrecognized(items, sequence, &timestamp, "codex", "record", kind, value);
            return;
        }
    }
    let payload = &value["payload"];
    match payload.get("type").and_then(Value::as_str) {
        Some("message") => {
            // Provider bootstrap instructions are not a visible chat turn.
            if !matches!(payload["role"].as_str(), Some("user" | "assistant")) {
                return;
            }
            let role = normalized_role(payload["role"].as_str());
            let message_id = payload["id"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| format!("native/{sequence}"));
            push_message(items, sequence, &timestamp, role, &message_id);
            match &payload["content"] {
                Value::Array(content) => {
                    for (offset, part) in content.iter().enumerate() {
                        let part_sequence = sequence + 1 + offset as u64;
                        if let Some(text) = part
                            .get("text")
                            .or_else(|| part.get("input_text"))
                            .or_else(|| part.get("output_text"))
                            .or_else(|| part.get("refusal"))
                            .and_then(Value::as_str)
                        {
                            push_content(items, part_sequence, &timestamp, role, text);
                            continue;
                        }
                        match part.get("type").and_then(Value::as_str) {
                            Some("input_image" | "output_image" | "image") => {
                                push_content(items, part_sequence, &timestamp, role, "[image]")
                            }
                            kind => push_unrecognized(
                                items,
                                part_sequence,
                                &timestamp,
                                "codex",
                                "message part",
                                kind,
                                part,
                            ),
                        }
                    }
                }
                Value::String(text) => push_content(items, sequence + 1, &timestamp, role, text),
                Value::Null => {}
                other => push_unrecognized(
                    items,
                    sequence + 1,
                    &timestamp,
                    "codex",
                    "message content",
                    Some(json_kind(other)),
                    other,
                ),
            }
        }
        Some("function_call" | "custom_tool_call") => push_tool_call(
            items,
            sequence,
            &timestamp,
            payload["call_id"].as_str().unwrap_or("native-call"),
            payload["name"].as_str().unwrap_or("tool"),
            payload
                .get("arguments")
                .or_else(|| payload.get("input"))
                .cloned()
                .unwrap_or_else(|| json!({})),
        ),
        Some("function_call_output" | "custom_tool_call_output") => push_tool_result(
            items,
            sequence,
            &timestamp,
            payload["call_id"].as_str().unwrap_or("native-call"),
            payload.get("output").cloned().unwrap_or(Value::Null),
        ),
        Some(kind) if CODEX_HIDDEN_ITEMS.contains(&kind) => {}
        kind => push_unrecognized(
            items,
            sequence,
            &timestamp,
            "codex",
            "response item",
            kind,
            payload,
        ),
    }
}

fn normalize_claude(
    value: &Value,
    sequence: u64,
    fallback_timestamp: &str,
    items: &mut Vec<Value>,
) {
    let timestamp =
        native_timestamp(value.get("timestamp")).unwrap_or_else(|| fallback_timestamp.to_owned());
    let role = match value.get("type").and_then(Value::as_str) {
        Some("user") => "user",
        Some("assistant") => "assistant",
        Some("attachment") => {
            // A prompt that arrives while Claude is busy, including a Small Talk delivery, is
            // recorded as a queued command rather than as a user entry. The other attachments
            // are context Claude injects for itself.
            if value.pointer("/attachment/type").and_then(Value::as_str) == Some("queued_command") {
                let message_id = value
                    .get("uuid")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("native/{sequence}"));
                push_message(items, sequence, &timestamp, "user", &message_id);
                push_claude_content(
                    items,
                    value.pointer("/attachment/prompt").unwrap_or(&Value::Null),
                    sequence + 1,
                    &timestamp,
                    "user",
                );
            }
            return;
        }
        Some("system") => {
            // Claude's own notices (compaction, local command output, API errors) carry text;
            // its timing and hook summaries do not.
            if let Some(text) = value.get("content").and_then(Value::as_str)
                && !text.trim().is_empty()
            {
                push_content(items, sequence, &timestamp, "system", text);
            }
            return;
        }
        Some(kind) if CLAUDE_BOOKKEEPING.contains(&kind) => return,
        kind => {
            push_unrecognized(items, sequence, &timestamp, "claude", "entry", kind, value);
            return;
        }
    };
    let message_id = value
        .pointer("/message/id")
        .and_then(Value::as_str)
        .or_else(|| value.get("uuid").and_then(Value::as_str))
        .map(str::to_owned)
        .unwrap_or_else(|| format!("native/{sequence}"));
    push_message(items, sequence, &timestamp, role, &message_id);
    push_claude_content(
        items,
        value.pointer("/message/content").unwrap_or(&Value::Null),
        sequence + 1,
        &timestamp,
        role,
    );
}

fn push_claude_content(
    items: &mut Vec<Value>,
    content: &Value,
    first_sequence: u64,
    timestamp: &str,
    role: &str,
) {
    match content {
        Value::String(text) => push_content(items, first_sequence, timestamp, role, text),
        Value::Array(parts) => {
            for (offset, part) in parts.iter().enumerate() {
                let item_sequence = first_sequence + offset as u64;
                match part.get("type").and_then(Value::as_str) {
                    Some("text") | None if part.get("text").is_some_and(Value::is_string) => {
                        push_content(
                            items,
                            item_sequence,
                            timestamp,
                            role,
                            part["text"].as_str().unwrap_or_default(),
                        );
                    }
                    Some("tool_use") => push_tool_call(
                        items,
                        item_sequence,
                        timestamp,
                        part["id"].as_str().unwrap_or("native-call"),
                        part["name"].as_str().unwrap_or("tool"),
                        part.get("input").cloned().unwrap_or_else(|| json!({})),
                    ),
                    Some("tool_result") => push_tool_result_with_status(
                        items,
                        item_sequence,
                        timestamp,
                        part["tool_use_id"].as_str().unwrap_or("native-call"),
                        part.get("content").cloned().unwrap_or(Value::Null),
                        part.get("is_error") == Some(&Value::Bool(true)),
                    ),
                    Some(kind) if HIDDEN_REASONING_BLOCKS.contains(&kind) => {}
                    Some(kind @ ("image" | "document")) => {
                        push_content(items, item_sequence, timestamp, role, &format!("[{kind}]"))
                    }
                    kind => push_unrecognized(
                        items,
                        item_sequence,
                        timestamp,
                        "claude",
                        "content block",
                        kind,
                        part,
                    ),
                }
            }
        }
        Value::Null => {}
        other => push_unrecognized(
            items,
            first_sequence,
            timestamp,
            "claude",
            "message content",
            Some(json_kind(other)),
            other,
        ),
    }
}

fn normalize_omp(
    driver: ExternalDriver,
    value: &Value,
    sequence: u64,
    fallback_timestamp: &str,
    items: &mut Vec<Value>,
) {
    let label = driver.as_str();
    let timestamp = native_timestamp(value.get("timestamp"))
        .or_else(|| native_timestamp(value.pointer("/message/timestamp")))
        .unwrap_or_else(|| fallback_timestamp.to_owned());
    match value.get("type").and_then(Value::as_str) {
        Some("message") => {}
        Some(kind @ ("compaction" | "branch_summary")) => {
            // A summary replaces the conversation before it in the model's context; show it as
            // the harness's own note.
            if let Some(summary) = value.get("summary").and_then(Value::as_str) {
                push_content(
                    items,
                    sequence,
                    &timestamp,
                    "system",
                    &format!("[{label} {kind}]\n{summary}"),
                );
            }
            return;
        }
        Some("custom_message") => {
            // An extension's message; `display: false` marks it hidden in the harness itself.
            if value.get("display") != Some(&Value::Bool(false)) {
                push_omp_content(
                    driver,
                    items,
                    value.get("content").unwrap_or(&Value::Null),
                    sequence,
                    &timestamp,
                    "system",
                );
            }
            return;
        }
        Some(kind) if OMP_BOOKKEEPING.contains(&kind) => return,
        kind => {
            push_unrecognized(items, sequence, &timestamp, label, "entry", kind, value);
            return;
        }
    }
    let message = &value["message"];
    let native_role = message.get("role").and_then(Value::as_str);
    let role = normalized_role(native_role);
    let message_id = value["id"]
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| format!("native/{sequence}"));
    push_message(items, sequence, &timestamp, role, &message_id);
    match native_role {
        Some("toolResult") if message.get("toolCallId").is_some_and(Value::is_string) => {
            push_tool_result_with_status(
                items,
                sequence + 1,
                &timestamp,
                message["toolCallId"].as_str().unwrap_or_default(),
                omp_result_images(driver, message.get("content").cloned().unwrap_or(Value::Null)),
                message.get("isError").and_then(Value::as_bool).unwrap_or(false),
            );
            return;
        }
        // A shell command the person ran with `!`: shown as the harness recorded it, without
        // attributing it to the agent.
        Some("bashExecution") => {
            let command = message
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let output = message
                .get("output")
                .and_then(Value::as_str)
                .unwrap_or_default();
            push_content(
                items,
                sequence + 1,
                &timestamp,
                "system",
                &format!("$ {command}\n{output}"),
            );
            return;
        }
        Some("branchSummary" | "compactionSummary")
            if message.get("content").is_none_or(Value::is_null) =>
        {
            if let Some(summary) = message.get("summary").and_then(Value::as_str) {
                push_content(items, sequence + 1, &timestamp, "system", summary);
            }
            return;
        }
        _ => {}
    }
    push_omp_content(
        driver,
        items,
        message.get("content").unwrap_or(&Value::Null),
        sequence + 1,
        &timestamp,
        role,
    );
}

fn push_omp_content(
    driver: ExternalDriver,
    items: &mut Vec<Value>,
    content: &Value,
    first_sequence: u64,
    timestamp: &str,
    role: &str,
) {
    let label = driver.as_str();
    let timestamp = timestamp.to_owned();
    match content {
        Value::String(text) => push_content(items, first_sequence, &timestamp, role, text),
        Value::Array(parts) => {
            for (offset, part) in parts.iter().enumerate() {
                let item_sequence = first_sequence + offset as u64;
                match part.get("type").and_then(Value::as_str) {
                    Some("text") | None if part.get("text").is_some_and(Value::is_string) => {
                        push_content(
                            items,
                            item_sequence,
                            &timestamp,
                            role,
                            part["text"].as_str().unwrap_or_default(),
                        );
                    }
                    Some("toolCall" | "tool_call") => push_tool_call(
                        items,
                        item_sequence,
                        &timestamp,
                        part.get("id")
                            .or_else(|| part.get("toolCallId"))
                            .and_then(Value::as_str)
                            .unwrap_or("native-call"),
                        part.get("name").and_then(Value::as_str).unwrap_or("tool"),
                        part.get("arguments").cloned().unwrap_or_else(|| json!({})),
                    ),
                    Some("toolResult" | "tool_result") => push_tool_result(
                        items,
                        item_sequence,
                        &timestamp,
                        part.get("toolCallId")
                            .or_else(|| part.get("call_id"))
                            .and_then(Value::as_str)
                            .unwrap_or("native-call"),
                        omp_result_images(driver, part.get("content").cloned().unwrap_or(Value::Null)),
                    ),
                    Some(kind) if HIDDEN_REASONING_BLOCKS.contains(&kind) => {}
                    Some("image") if driver == ExternalDriver::Omp => {
                        push_omp_image_unavailable(items, item_sequence, &timestamp, part);
                    }
                    Some("image") => {
                        push_content(items, item_sequence, &timestamp, role, "[image]")
                    }
                    kind => push_unrecognized(
                        items,
                        item_sequence,
                        &timestamp,
                        label,
                        "content block",
                        kind,
                        part,
                    ),
                }
            }
        }
        Value::Null => {}
        other => push_unrecognized(
            items,
            first_sequence,
            &timestamp,
            label,
            "message content",
            Some(json_kind(other)),
            other,
        ),
    }
}

fn omp_image_availability(image: &Value) -> Value {
    let reference = image.get("data").and_then(Value::as_str).filter(|reference| {
        reference.strip_prefix("blob:sha256:").is_some_and(|digest| {
            digest.len() == 64
                && digest.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
    });
    let mut details = json!({
        "_tag":"OmpImage","version":1,
        "availability":if reference.is_some() { "unavailable" } else { "withheld" },
        "reason":if reference.is_some() { "native_blob_not_fetchable" } else { "image_payload_not_authorized" },
    });
    if let Some(reference) = reference {
        // This is not a st attachment ID: no origin/authorization evidence exists to fetch it.
        details["native_ref"] = json!(reference);
    }
    if let Some(mime) = image.get("mimeType").and_then(Value::as_str)
        .filter(|mime| matches!(*mime, "image/png" | "image/jpeg" | "image/gif" | "image/webp"))
    {
        details["mime_type"] = json!(mime);
    } else {
        details["mime_availability"] = json!("unknown");
    }
    for key in ["width", "height"] {
        if let Some(value) = image.get(key).and_then(Value::as_u64).filter(|value| *value <= 65_535) {
            details[key] = json!(value);
        }
    }
    details
}

fn push_omp_image_unavailable(
    items: &mut Vec<Value>,
    sequence: u64,
    timestamp: &str,
    image: &Value,
) {
    items.push(timeline_item(sequence, timestamp, "system", "error", json!({
        "code":"native_image_unavailable",
        "message":"Native image is unavailable through the conversation attachment service.",
        "retryable":false,
        "details":omp_image_availability(image),
    })));
}

/// Unknown native shapes must not turn an image or a data URI into visible JSON text.
fn omp_has_image_payload(value: &Value) -> bool {
    match value {
        Value::String(text) => text.trim_start().starts_with("data:"),
        Value::Array(values) => values.iter().any(omp_has_image_payload),
        Value::Object(fields) => {
            fields.get("type").and_then(Value::as_str).is_some_and(|kind| {
                matches!(kind, "image" | "input_image" | "image_url")
            }) || fields.contains_key("image_url")
                || fields.values().any(omp_has_image_payload)
        }
        _ => false,
    }
}

fn omp_result_images(driver: ExternalDriver, content: Value) -> Value {
    if driver != ExternalDriver::Omp {
        return content;
    }
    match content {
        Value::Array(parts) => Value::Array(
            parts.into_iter().map(|part| omp_result_images(driver, part)).collect(),
        ),
        other if omp_has_image_payload(&other) => omp_image_availability(&other),
        other => other,
    }
}

fn normalized_role(role: Option<&str>) -> &'static str {
    match role {
        Some("user") => "user",
        Some("assistant") => "assistant",
        Some("tool" | "toolResult" | "tool_result") => "tool",
        _ => "system",
    }
}

fn push_message(
    items: &mut Vec<Value>,
    sequence: u64,
    timestamp: &str,
    role: &str,
    message_id: &str,
) {
    items.push(timeline_item(
        sequence,
        timestamp,
        role,
        "message",
        json!({"message_id": message_id}),
    ));
}

fn push_content(items: &mut Vec<Value>, sequence: u64, timestamp: &str, role: &str, text: &str) {
    let text = bounded_text(text);
    items.push(timeline_item(
        sequence,
        timestamp,
        role,
        "content",
        json!({"media_type":"text/plain", "text":text}),
    ));
}

fn push_tool_call(
    items: &mut Vec<Value>,
    sequence: u64,
    timestamp: &str,
    call_id: &str,
    name: &str,
    arguments: Value,
) {
    let arguments = arguments
        .as_str()
        .and_then(|value| serde_json::from_str(value).ok())
        .unwrap_or(arguments);
    let arguments = bounded_value(arguments);
    items.push(timeline_item(
        sequence,
        timestamp,
        "assistant",
        "tool_call",
        json!({"call_id":call_id, "name":name, "arguments":arguments}),
    ));
}

fn push_tool_result(
    items: &mut Vec<Value>,
    sequence: u64,
    timestamp: &str,
    call_id: &str,
    content: Value,
) {
    push_tool_result_with_status(items, sequence, timestamp, call_id, content, false);
}

fn push_tool_result_with_status(
    items: &mut Vec<Value>,
    sequence: u64,
    timestamp: &str,
    call_id: &str,
    content: Value,
    failed: bool,
) {
    let content = bounded_value(content);
    let status = if failed { "error" } else { "success" };
    items.push(timeline_item(sequence, timestamp, "tool", "tool_result", json!({"call_id":call_id, "status":status, "media_type":"application/json", "content":content})));
}

fn truncate_at_char_boundary(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

fn bounded_text(value: &str) -> String {
    if value.len() <= MAX_TIMELINE_VALUE_BYTES {
        return value.to_owned();
    }
    // The bound is in bytes: it keeps a page under the gateway's response ceiling.
    let mut output = truncate_at_char_boundary(value, MAX_TIMELINE_VALUE_BYTES).to_owned();
    output.push_str("\n[st truncated this native timeline value]");
    output
}

fn bounded_value(value: Value) -> Value {
    match serde_json::to_string(&value) {
        Ok(encoded) if encoded.len() > MAX_TIMELINE_VALUE_BYTES => {
            Value::String(bounded_text(&encoded))
        }
        _ => value,
    }
}

fn timeline_item(
    sequence: u64,
    timestamp: &str,
    role: &str,
    entry_type: &str,
    body: Value,
) -> Value {
    json!({
        "id": format!("timeline-entry/native-{sequence}"),
        "sequence": sequence,
        "revision": 1,
        "timestamp": timestamp,
        "role": role,
        "type": entry_type,
        "final": true,
        "body": body
    })
}

fn parse_timestamp(value: Option<&Value>) -> Option<u128> {
    let value = value?.as_str()?;
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|value| value.timestamp_millis().max(0) as u128)
}

fn system_time_ms(value: SystemTime) -> u128 {
    value
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn digest(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))
}

/// Test support shared with the client API tests.
#[cfg(all(test, target_os = "linux"))]
pub(crate) mod test_support {
    use super::*;

    /// A stand-in for `st3 driver claude --subject <subject>` with one child, as the process
    /// fallback sees it: a shell whose arguments carry the driver's options.
    pub(crate) struct FakeClaudeDriver {
        pub(crate) driver: std::process::Child,
        pub(crate) child: u32,
    }

    impl FakeClaudeDriver {
        pub(crate) fn start(subject: &str) -> Self {
            let bash = std::env::var_os("PATH")
                .into_iter()
                .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
                .map(|directory| directory.join("bash"))
                .find(|candidate| candidate.is_file())
                .expect("the test environment must provide `bash` on PATH");
            let driver = std::process::Command::new(bash)
                .args([
                    "-c",
                    "sleep 30 & wait",
                    "st3",
                    "driver",
                    "claude",
                    "--subject",
                    subject,
                    "--",
                    "claude",
                ])
                .spawn()
                .unwrap();
            let child = (0..200)
                .find_map(|_| {
                    let found = linux_child_processes(driver.id()).into_iter().next();
                    if found.is_none() {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    found
                })
                .expect("the stand-in driver should start its child");
            Self { driver, child }
        }

        /// The evidence token the Claude wrapper inside this driver would mint now.
        pub(crate) fn token(&self) -> String {
            let now = system_time_ms(SystemTime::now());
            format!("{}-{now}-0", self.driver.id())
        }

        /// Write the `~/.claude/sessions/<pid>.json` record Claude keeps for its process,
        /// with the child's real kernel start time unless `start` overrides it.
        pub(crate) fn record_session(&self, home: &Path, session: &str, start: Option<&str>) {
            let sessions = home.join(".claude/sessions");
            fs::create_dir_all(&sessions).unwrap();
            let start = start
                .map(str::to_owned)
                .unwrap_or_else(|| linux_process_start_ticks(self.child).unwrap());
            fs::write(
                sessions.join(format!("{}.json", self.child)),
                json!({"pid":self.child,"sessionId":session,"procStart":start}).to_string(),
            )
            .unwrap();
        }
    }

    impl Drop for FakeClaudeDriver {
        fn drop(&mut self) {
            unsafe { libc::kill(self.child as i32, libc::SIGKILL) };
            let _ = self.driver.kill();
            let _ = self.driver.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_cache_refreshes_after_a_transcript_changes() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("session.jsonl");
        fs::write(
            &path,
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"first\"}}\n",
        )
        .unwrap();
        let first = read_metadata(ExternalDriver::Codex, &path)
            .unwrap()
            .unwrap();
        assert_eq!(first.native_id, "first");
        assert_eq!(
            read_metadata(ExternalDriver::Claude, &path)
                .unwrap()
                .unwrap()
                .native_id,
            "session"
        );
        assert_eq!(
            read_metadata(ExternalDriver::Codex, &path)
                .unwrap()
                .unwrap()
                .revision,
            first.revision
        );
        fs::write(
            &path,
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"second-longer\"}}\n",
        )
        .unwrap();
        let second = read_metadata(ExternalDriver::Codex, &path)
            .unwrap()
            .unwrap();
        assert_eq!(second.native_id, "second-longer");
        assert_ne!(second.revision, first.revision);
    }

    #[test]
    fn one_unreadable_session_line_does_not_fail_discovery_or_hide_the_session() {
        let home = tempfile::tempdir().unwrap();
        let project = home.path().join(".claude/projects/example");
        fs::create_dir_all(&project).unwrap();
        // A line that is not UTF-8 costs only that line: Claude's file name still names the
        // session, and the later, readable header still supplies its workspace.
        fs::write(
            project.join("broken-id.jsonl"),
            [
                &b"\xff\xfe not UTF-8\n"[..],
                br#"{"sessionId":"broken-id","cwd":"/work","type":"user","message":{"content":"hi"}}"#,
            ]
            .concat(),
        )
        .unwrap();
        fs::write(
            project.join("good-id.jsonl"),
            r#"{"sessionId":"good-id","cwd":"/tmp","timestamp":"2026-09-24T00:00:00Z","type":"user","message":{"content":"hello"}}"#,
        )
        .unwrap();
        let mut found = discover_files(home.path(), true, &[]).unwrap();
        found.sort_by(|left, right| left.native_id.cmp(&right.native_id));
        assert_eq!(
            found
                .iter()
                .map(|session| session.native_id.as_str())
                .collect::<Vec<_>>(),
            ["broken-id", "good-id"]
        );
        assert_eq!(found[0].cwd.as_deref(), Some(Path::new("/work")));
    }

    #[test]
    fn omp_inventory_skips_nested_tool_logs_but_keeps_project_transcripts() {
        let home = tempfile::tempdir().unwrap();
        for provider in [".omp", ".oh-omp"] {
            let project = home.path().join(provider).join("agent/sessions/project");
            let attachment = project.join("2026-09-30T00-00-00Z_session-id");
            fs::create_dir_all(&attachment).unwrap();
            fs::write(
                project.join("2026-09-30T00-00-00Z_session-id.jsonl"),
                "{\"type\":\"session\",\"id\":\"session-id\",\"cwd\":\"/tmp\"}\n",
            )
            .unwrap();
            fs::write(
                attachment.join("tool.jsonl"),
                "{\"type\":\"session\",\"id\":\"not-a-transcript\",\"cwd\":\"/tmp\"}\n",
            )
            .unwrap();
        }
        let found = discover_files(home.path(), true, &[]).unwrap();
        assert_eq!(found.len(), 2);
        assert!(found.iter().all(|item| item.native_id == "session-id"));
        assert!(
            found
                .iter()
                .all(|item| item.transcript.file_name().unwrap() != "tool.jsonl")
        );
    }

    #[test]
    fn native_only_inventory_reads_explicit_live_transcripts_not_history_or_attachments() {
        let home = tempfile::tempdir().unwrap();
        let project = home.path().join(".omp/agent/sessions/project");
        let attachment = project.join("attachments");
        fs::create_dir_all(&attachment).unwrap();
        let active = project.join("2026-09-30T00-00-00Z_active.jsonl");
        let historical = project.join("2026-08-01T00-00-00Z_historical.jsonl");
        let nested = attachment.join("tool.jsonl");
        for (path, id) in [
            (&active, "active"),
            (&historical, "historical"),
            (&nested, "not-a-session"),
        ] {
            fs::write(path, format!("{{\"type\":\"session\",\"id\":\"{id}\"}}\n")).unwrap();
        }
        let candidate = |command: String| ProcessCandidate {
            driver: ExternalDriver::Omp,
            managed_by_st3: false,
            process: ExternalProcess {
                pid: 42,
                parent_pid: 1,
                started_at_unix_ms: 1,
                fingerprint: "live".into(),
                cwd: None,
                command,
                exact_session: false,
            },
        };
        let candidates = [
            candidate(format!("omp --resume={}", active.display())),
            candidate(format!("omp --log {}", nested.display())),
            candidate("omp --resume historical".into()),
        ];
        let found = discover_files(home.path(), false, &candidates).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].native_id, "active");
        assert_eq!(found[0].transcript, active);
    }

    #[cfg(unix)]
    #[test]
    fn native_only_inventory_accepts_a_physical_path_beneath_symlinked_session_root() {
        let home = tempfile::tempdir().unwrap();
        let physical = home.path().join("cold-storage/2026/09/30");
        fs::create_dir_all(&physical).unwrap();
        fs::create_dir_all(home.path().join(".codex")).unwrap();
        std::os::unix::fs::symlink(
            home.path().join("cold-storage"),
            home.path().join(".codex/sessions"),
        )
        .unwrap();
        let transcript = physical.join("rollout-live.jsonl");
        fs::write(
            &transcript,
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"live\"}}\n",
        )
        .unwrap();
        let candidate = ProcessCandidate {
            driver: ExternalDriver::Codex,
            managed_by_st3: false,
            process: ExternalProcess {
                pid: 42,
                parent_pid: 1,
                started_at_unix_ms: 1,
                fingerprint: "live".into(),
                cwd: None,
                command: format!("codex resume {}", transcript.display()),
                exact_session: false,
            },
        };
        let found = discover_files(home.path(), false, &[candidate]).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].native_id, "live");
    }

    #[cfg(unix)]
    #[test]
    fn native_only_inventory_does_not_follow_a_transcript_symlink_outside_provider_root() {
        let home = tempfile::tempdir().unwrap();
        let project = home.path().join(".omp/agent/sessions/project");
        fs::create_dir_all(&project).unwrap();
        let outside = home.path().join("outside.jsonl");
        fs::write(&outside, "{\"type\":\"session\",\"id\":\"outside\"}\n").unwrap();
        let alias = project.join("escape.jsonl");
        std::os::unix::fs::symlink(&outside, &alias).unwrap();
        let candidate = ProcessCandidate {
            driver: ExternalDriver::Omp,
            managed_by_st3: false,
            process: ExternalProcess {
                pid: 42,
                parent_pid: 1,
                started_at_unix_ms: 1,
                fingerprint: "live".into(),
                cwd: None,
                command: format!("omp --resume={}", alias.display()),
                exact_session: false,
            },
        };
        assert!(
            discover_files(home.path(), false, &[candidate])
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn historical_inventory_after_native_only_read_is_not_served_from_running_cache() {
        let home = tempfile::tempdir().unwrap();
        let project = home.path().join(".omp/agent/sessions/project");
        fs::create_dir_all(&project).unwrap();
        fs::write(
            project.join("2026-09-30T00-00-00Z_saved-only-case.jsonl"),
            "{\"type\":\"session\",\"id\":\"saved-only-case\"}\n",
        )
        .unwrap();
        assert!(
            discover(Some(home.path()), false)
                .unwrap()
                .sessions
                .is_empty()
        );
        let saved = discover(Some(home.path()), true).unwrap();
        assert_eq!(saved.sessions.len(), 1);
        assert_eq!(saved.sessions[0].native_id, "saved-only-case");
    }

    #[test]
    fn saved_history_request_does_not_wait_past_bound_for_a_stuck_transcript_read() {
        let home = tempfile::tempdir().unwrap();
        let saved = SessionMetadata {
            driver: ExternalDriver::Omp,
            native_id: "saved-before-stall".into(),
            transcript: home.path().join("saved.jsonl"),
            cwd: None,
            title: None,
            started_at_unix_ms: 1,
            updated_at_unix_ms: 1,
            revision: "1".into(),
        };
        // A background read that never finishes, as on cold, seek-saturated storage.
        let stall = |files| {
            HISTORY.0.lock().unwrap().insert(
                home.path().to_owned(),
                HistoryInventory {
                    files,
                    refreshing: true,
                    ..HistoryInventory::default()
                },
            );
        };

        stall(None);
        let started = Instant::now();
        let error = historical_files(home.path()).unwrap_err().to_string();
        assert!(error.contains("retry shortly"), "{error}");
        assert!(started.elapsed() < HISTORY_WAIT + Duration::from_secs(1));

        let expired = Instant::now().checked_sub(DISCOVERY_CACHE_TTL * 2).unwrap();
        stall(Some((expired, Arc::new(vec![saved]))));
        let started = Instant::now();
        let files = historical_files(home.path()).unwrap();
        assert_eq!(files[0].native_id, "saved-before-stall");
        assert!(started.elapsed() < HISTORY_WAIT + Duration::from_secs(1));
    }

    #[test]
    fn duplicate_native_id_prefers_live_workspace_then_newest_and_refuses_ties() {
        let root = tempfile::tempdir().unwrap();
        let active = root.path().join("active");
        let stale = root.path().join("stale");
        fs::create_dir_all(&active).unwrap();
        fs::create_dir_all(&stale).unwrap();
        let metadata = |workspace: &Path, modified| SessionMetadata {
            driver: ExternalDriver::Omp,
            native_id: "shared-id".into(),
            transcript: workspace.join("shared-id.jsonl"),
            cwd: Some(workspace.to_owned()),
            title: None,
            started_at_unix_ms: 1,
            updated_at_unix_ms: modified,
            revision: format!("{modified}"),
        };
        let process = ProcessCandidate {
            driver: ExternalDriver::Omp,
            managed_by_st3: false,
            process: ExternalProcess {
                pid: 42,
                parent_pid: 1,
                started_at_unix_ms: 1,
                fingerprint: "fingerprint".into(),
                cwd: Some(active.clone()),
                command: "agent-omp --resume shared-id".into(),
                exact_session: true,
            },
        };
        let id = external_session_id(ExternalDriver::Omp, "shared-id");
        let selected = resolve_duplicate_sessions(
            vec![metadata(&active, 10), metadata(&stale, 20)],
            &[process.clone()],
            Some(&id),
        )
        .unwrap();
        assert_eq!(selected[0].transcript, active.join("shared-id.jsonl"));

        #[cfg(unix)]
        {
            let alias = root.path().join("old-workspace-symlink");
            std::os::unix::fs::symlink(&active, &alias).unwrap();
            let selected = resolve_duplicate_sessions(
                vec![metadata(&alias, 10), metadata(&stale, 20)],
                &[process.clone()],
                Some(&id),
            )
            .unwrap();
            assert_eq!(selected[0].transcript, alias.join("shared-id.jsonl"));
        }

        let selected = resolve_duplicate_sessions(
            vec![metadata(&active, 10), metadata(&stale, 20)],
            &[],
            Some(&id),
        )
        .unwrap();
        assert_eq!(selected[0].transcript, stale.join("shared-id.jsonl"));

        let mut relocated_copy = metadata(&stale, 20);
        relocated_copy.cwd = Some(active.clone());
        let selected = resolve_duplicate_sessions(
            vec![metadata(&active, 10), relocated_copy],
            &[process.clone()],
            Some(&id),
        )
        .unwrap();
        assert_eq!(selected[0].transcript, stale.join("shared-id.jsonl"));

        let other = root.path().join("other");
        fs::create_dir_all(&other).unwrap();
        let ambiguous = vec![metadata(&active, 10), metadata(&other, 10)];
        assert!(
            resolve_duplicate_sessions(ambiguous.clone(), &[], Some(&id))
                .unwrap_err()
                .to_string()
                .contains("refusing ambiguous import")
        );
        assert_eq!(
            resolve_duplicate_sessions(ambiguous, &[], None)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn imported_omp_timeline_reads_only_the_claimed_transcript() {
        let root = tempfile::tempdir().unwrap();
        let selected = root.path().join("selected.jsonl");
        let stale = root.path().join("stale.jsonl");
        for (path, content) in [(&selected, "resumed context"), (&stale, "stale context")] {
            fs::write(
                path,
                format!(
                    "{}\n{}\n",
                    json!({"type":"session","id":"shared-id","cwd":root.path()}),
                    json!({"type":"message","id":"m1","message":{"role":"user","content":content}})
                ),
            )
            .unwrap();
        }
        let bound = find_bound_pi_family_transcript(ExternalDriver::Omp, &selected, "shared-id")
            .unwrap()
            .unwrap();
        let timeline = normalized_timeline(&bound).unwrap();
        assert!(
            timeline
                .iter()
                .any(|item| item["type"] == "content" && item["body"]["text"] == "resumed context")
        );
        assert!(
            !timeline
                .iter()
                .any(|item| item["body"]["text"] == "stale context")
        );
        assert!(
            find_bound_pi_family_transcript(ExternalDriver::Omp, &stale, "wrong-id")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn bound_claude_transcript_resolves_without_a_fleetwide_discovery() {
        let home = tempfile::tempdir().unwrap();
        let project = home.path().join(".claude/projects/example");
        fs::create_dir_all(&project).unwrap();
        fs::write(
            project.join("bound-id.jsonl"),
            r#"{"sessionId":"bound-id","timestamp":"2026-09-24T00:00:00Z","type":"user","message":{"content":"hello"}}"#,
        )
        .unwrap();
        let session = find_bound_transcript(home.path(), ExternalDriver::Claude, "bound-id")
            .unwrap()
            .unwrap();
        assert_eq!(session.native_id, "bound-id");
        assert_eq!(session.driver, ExternalDriver::Claude);
        assert!(session.transcript.ends_with("bound-id.jsonl"));
        assert!(
            find_bound_transcript(home.path(), ExternalDriver::Claude, "other-id")
                .unwrap()
                .is_none()
        );
    }

    #[cfg(target_os = "linux")]
    fn path_executable(name: &str) -> PathBuf {
        std::env::var_os("PATH")
            .into_iter()
            .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
            .map(|directory| directory.join(name))
            .find(|candidate| candidate.is_file())
            .unwrap_or_else(|| panic!("the test environment must provide `{name}` on PATH"))
    }

    #[test]
    fn codex_claude_pi_and_omp_transcripts_normalize_to_one_timeline_shape() {
        let fallback = timestamp(0);
        let mut items = Vec::new();
        normalize_codex(
            &json!({"type":"response_item","timestamp":"2026-01-01T00:00:00Z","payload":{"type":"message","role":"assistant","id":"m1","content":[{"type":"output_text","text":"codex"}]}}),
            0,
            &fallback,
            &mut items,
        );
        normalize_claude(
            &json!({"type":"assistant","timestamp":"2026-01-01T00:00:01Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"c1","name":"shell","input":{"command":"true"}}]}}),
            16,
            &fallback,
            &mut items,
        );
        normalize_omp(
            ExternalDriver::Omp,
            &json!({"type":"message","id":"m2","timestamp":"2026-01-01T00:00:02Z","message":{"role":"tool","content":[{"type":"toolResult","toolCallId":"c1","content":"ok"}]}}),
            32,
            &fallback,
            &mut items,
        );
        assert!(
            items
                .iter()
                .any(|item| item["type"] == "content" && item["body"]["text"] == "codex")
        );
        assert!(
            items
                .iter()
                .any(|item| item["type"] == "tool_call" && item["body"]["call_id"] == "c1")
        );
        assert!(
            items
                .iter()
                .any(|item| item["type"] == "tool_result" && item["body"]["call_id"] == "c1")
        );
    }

    // Native OMP call/result pairs captured on 2026-10-02, with tool input/output redacted.
    fn omp_tool_result_fixture() -> Vec<Value> {
        include_str!("../fixtures/omp-tool-results.jsonl")
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[test]
    fn omp_images_keep_safe_identity_and_typed_unavailability_without_pixels() {
        let reference = format!("blob:sha256:{}", "a".repeat(64));
        let image = json!({"type":"image","data":reference,"mimeType":"image/webp","width":640,"height":480});
        let mut items = Vec::new();
        normalize_omp(ExternalDriver::Omp, &json!({
            "type":"message","id":"image-message","message":{"role":"user","content":[image.clone()]}
        }), 0, "", &mut items);
        let notice = items.iter().find(|item| item["type"] == "error").unwrap();
        assert_eq!(notice["body"]["code"], "native_image_unavailable");
        assert_eq!(notice["body"]["details"]["native_ref"], reference);
        assert_eq!(notice["body"]["details"]["mime_type"], "image/webp");
        assert_eq!(notice["body"]["details"]["availability"], "unavailable");
        assert_eq!(notice["body"]["details"]["reason"], "native_blob_not_fetchable");
        assert_eq!(notice["body"]["details"]["width"], 640);
        assert_eq!(notice["body"]["details"]["height"], 480);

        for data in ["planted-image-bytes", "https://user:secret@example.invalid/image", "blob:sha256:secret"] {
            items.clear();
            normalize_omp(ExternalDriver::Omp, &json!({
                "type":"message","id":"image-tool","message":{
                    "role":"toolResult","toolCallId":"image-call","isError":false,
                    "content":[{"type":"text","text":"kept"},{"type":"image","data":data,"mimeType":"image/png"}]
                }
            }), 0, "", &mut items);
            let result = items.iter().find(|item| item["type"] == "tool_result").unwrap();
            assert_eq!(result["body"]["call_id"], "image-call");
            assert_eq!(result["body"]["content"][0]["text"], "kept");
            assert_eq!(result["body"]["content"][1]["_tag"], "OmpImage");
            assert_eq!(result["body"]["content"][1]["availability"], "withheld");
            assert_eq!(result["body"]["content"][1]["mime_type"], "image/png");
            assert!(!serde_json::to_string(&items).unwrap().contains(data));
        }
    }

    #[test]
    fn omp_nested_result_images_withhold_bytes_and_unknown_mime() {
        let mut items = Vec::new();
        normalize_omp(ExternalDriver::Omp, &json!({
            "type":"message","id":"nested-image","message":{"role":"tool","content":[{
                "type":"toolResult","toolCallId":"nested-call","content":[{
                    "type":"image","data":"planted-image-bytes","mimeType":"planted-mime-secret"
                }]
            }]}
        }), 0, "", &mut items);
        let result = items.iter().find(|item| item["type"] == "tool_result").unwrap();
        assert_eq!(result["body"]["content"][0]["mime_availability"], "unknown");
        assert_eq!(result["body"]["content"][0]["reason"], "image_payload_not_authorized");
        assert!(!serde_json::to_string(&items).unwrap().contains("planted-"));
    }

    #[test]
    fn omp_images_reject_malformed_direct_refs_and_oversized_dimensions() {
        let canonical = format!("blob:sha256:{}", "a".repeat(64));
        for reference in [
            format!("blob:sha256:{}", "A".repeat(64)),
            format!("blob:sha256:{}", "a".repeat(63)),
            format!("blob:sha256:{}", "a".repeat(65)),
            format!("{canonical}trailing-data"),
            format!("blob:sha256:{}", "é".repeat(32)),
            "data:image/png;base64,planted-pixels".into(),
        ] {
            let mut items = Vec::new();
            normalize_omp(ExternalDriver::Omp, &json!({
                "type":"message","id":"malformed-image","message":{"role":"user","content":[{
                    "type":"image","data":reference,"mimeType":"planted-mime",
                    "width":9_007_199_254_740_992_u64,"height":65_536
                }]}
            }), 0, "", &mut items);
            let details = &items.iter().find(|item| item["type"] == "error").unwrap()["body"]["details"];
            assert_eq!(details["availability"], "withheld");
            assert_eq!(details["reason"], "image_payload_not_authorized");
            assert!(details.get("native_ref").is_none());
            assert!(details.get("width").is_none());
            assert!(details.get("height").is_none());
            assert!(!serde_json::to_string(&items).unwrap().contains(&reference));
        }
        let details = omp_image_availability(&json!({
            "type":"image","data":canonical,"width":65_535,"height":480
        }));
        assert_eq!(details["width"], 65_535);
        assert_eq!(details["height"], 480);
    }

    #[test]
    fn omp_images_withhold_unrecognized_and_single_object_payloads() {
        let payload = "data:image/png;base64,planted-pixels";
        for block in [
            json!({"type":"input_image","image_url":payload}),
            json!({"type":"image_url","image_url":{"url":payload}}),
            json!({"type":"future_block","source":{"data":payload}}),
        ] {
            for content in [json!([block.clone()]), block.clone()] {
                let mut items = Vec::new();
                normalize_omp(ExternalDriver::Omp, &json!({
                    "type":"message","id":"unknown-image","message":{"role":"user","content":content}
                }), 0, "", &mut items);
                let details = &items.iter().find(|item| item["type"] == "error").unwrap()["body"]["details"];
                assert_eq!(details["_tag"], "OmpImage");
                assert_eq!(details["availability"], "withheld");
                assert!(!serde_json::to_string(&items).unwrap().contains(payload));
            }
            for content in [json!([{"type":"text","text":"kept"},block.clone()]), block.clone()] {
                let mut items = Vec::new();
                normalize_omp(ExternalDriver::Omp, &json!({
                    "type":"message","id":"unknown-tool-image","message":{
                        "role":"toolResult","toolCallId":"image-call","content":content
                    }
                }), 0, "", &mut items);
                let result = &items.iter().find(|item| item["type"] == "tool_result").unwrap()["body"]["content"];
                let placeholder = if result.is_array() {
                    assert_eq!(result[0]["text"], "kept");
                    &result[1]
                } else {
                    result
                };
                assert_eq!(placeholder["_tag"], "OmpImage");
                assert_eq!(placeholder["availability"], "withheld");
                assert!(!serde_json::to_string(&items).unwrap().contains(payload));
            }
        }
    }

    #[test]
    fn pi_images_remain_plain_image_placeholders() {
        let mut items = Vec::new();
        normalize_omp(ExternalDriver::Pi, &json!({
            "type":"message","id":"pi-image","message":{"role":"user","content":[{
                "type":"image","data":"planted-pixels","mimeType":"image/png"
            }]}
        }), 0, "", &mut items);
        let content = items.iter().find(|item| item["type"] == "content").unwrap();
        assert_eq!(content["body"]["text"], "[image]");
        assert!(!serde_json::to_string(&items).unwrap().contains("planted-pixels"));
    }

    #[test]
    fn omp_message_tool_result_success_correlates_with_call() {
        let fixture = omp_tool_result_fixture();
        let mut items = Vec::new();
        for (offset, entry) in fixture[..2].iter().enumerate() {
            normalize_omp(ExternalDriver::Omp, entry, offset as u64 * 16, "", &mut items);
        }
        let call = items.iter().find(|item| item["type"] == "tool_call").unwrap();
        let result = items.iter().find(|item| item["type"] == "tool_result").unwrap();
        assert_eq!(result["body"]["call_id"], call["body"]["call_id"]);
        assert_eq!(result["body"]["status"], "success");
        assert_eq!(result["body"]["content"], fixture[1]["message"]["content"]);
        assert_eq!(result["timestamp"], fixture[1]["timestamp"]);
    }

    #[test]
    fn omp_message_tool_result_error_retains_status_and_text() {
        let fixture = omp_tool_result_fixture();
        let mut items = Vec::new();
        for (offset, entry) in fixture[2..].iter().enumerate() {
            normalize_omp(ExternalDriver::Omp, entry, offset as u64 * 16, "", &mut items);
        }
        let call = items.iter().find(|item| item["type"] == "tool_call").unwrap();
        let result = items.iter().find(|item| item["type"] == "tool_result").unwrap();
        assert_eq!(result["body"]["call_id"], call["body"]["call_id"]);
        assert_eq!(result["body"]["status"], "error");
        assert_eq!(result["body"]["content"], fixture[3]["message"]["content"]);
    }

    #[test]
    fn omp_message_tool_result_legacy_block_remains_correlated() {
        let mut entry = omp_tool_result_fixture()[1].clone();
        let message = entry["message"].as_object_mut().unwrap();
        let call_id = message.remove("toolCallId").unwrap();
        let content = message.remove("content").unwrap();
        message.insert("content".to_owned(), json!([{
            "type": "toolResult", "toolCallId": call_id, "content": content
        }]));
        let mut items = Vec::new();
        normalize_omp(ExternalDriver::Omp, &entry, 0, "", &mut items);
        let result = items.iter().find(|item| item["type"] == "tool_result").unwrap();
        assert_eq!(result["body"]["call_id"], call_id);
        assert_eq!(result["body"]["status"], "success");
        assert_eq!(result["body"]["content"], content);
    }

    #[test]
    fn current_codex_custom_tools_are_visible_but_bootstrap_prompts_are_not() {
        let fallback = timestamp(0);
        let mut items = Vec::new();
        for (sequence, payload) in [
            (
                16,
                json!({"type":"message","role":"developer","id":"hidden","content":[{"type":"input_text","text":"private bootstrap"}]}),
            ),
            (
                32,
                json!({"type":"custom_tool_call","call_id":"call-one","name":"exec","input":"const task = 1;"}),
            ),
            (
                48,
                json!({"type":"custom_tool_call_output","call_id":"call-one","output":[{"type":"input_text","text":"done"}]}),
            ),
        ] {
            normalize_codex(
                &json!({"type":"response_item","timestamp":"2026-09-24T12:00:00Z","payload":payload}),
                sequence,
                &fallback,
                &mut items,
            );
        }
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["type"], "tool_call");
        assert_eq!(items[0]["body"]["call_id"], "call-one");
        assert_eq!(items[1]["type"], "tool_result");
        assert_eq!(items[1]["body"]["content"][0]["text"], "done");
    }

    #[test]
    fn process_detection_uses_executables_not_incidental_arguments() {
        assert_eq!(
            driver_for_command("/usr/bin/codex resume abc"),
            Some(ExternalDriver::Codex)
        );
        assert_eq!(
            driver_for_command("node /opt/bin/claude --resume abc"),
            Some(ExternalDriver::Claude)
        );
        assert_eq!(
            driver_for_command("/opt/bin/pi --session abc"),
            Some(ExternalDriver::Pi)
        );
        assert_eq!(
            driver_for_command("/opt/st3 driver omp --subject agent/x -- omp"),
            Some(ExternalDriver::Omp)
        );
        assert_eq!(
            driver_for_command("/Users/example/.opencode/bin/opencode --session ses_123"),
            Some(ExternalDriver::OpenCode)
        );
        assert_eq!(driver_for_command("rg codex crates/st3"), None);
        assert_eq!(driver_for_command("bash -c echo claude"), None);
    }

    #[test]
    fn omp_worker_modes_are_not_harness_sessions() {
        for mode in [
            "__omp_worker_daemon_broker",
            "__omp_worker_js_eval_process",
            "__omp_worker_lsp_mux",
            "__omp_worker_text_predict",
            "__omp_worker_future_helper",
        ] {
            for entry in ["/opt/bin/omp", "node /opt/bin/omp", "bun /opt/bin/omp"] {
                let command = format!("{entry} {mode}");
                assert_eq!(driver_for_command(&command), None, "{command}");
            }
        }
    }

    #[test]
    fn genuine_omp_sessions_are_admitted() {
        for command in [
            "omp",
            "/opt/bin/omp --resume native-session",
            "node /opt/bin/omp",
            "node /opt/bin/omp --resume native-session",
            "bun /opt/bin/omp",
            "omp --resume __omp_worker_not_a_mode",
        ] {
            assert_eq!(driver_for_command(command), Some(ExternalDriver::Omp));
        }
    }

    #[test]
    fn opencode_sqlite_sessions_are_discovered_and_normalized() {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let parent = home.path().join(".local/share/opencode");
        fs::create_dir_all(&parent).unwrap();
        let database = parent.join("opencode.db");
        let connection = Connection::open(&database).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE session (\
                    id TEXT PRIMARY KEY, directory TEXT, title TEXT, \
                    time_created INTEGER, time_updated INTEGER\
                 );\
                 CREATE TABLE message (\
                    id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, \
                    time_updated INTEGER, data TEXT\
                 );\
                 CREATE TABLE part (\
                    id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT, \
                    time_created INTEGER, time_updated INTEGER, data TEXT\
                 );",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO session VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    "ses_native",
                    workspace.path().to_string_lossy(),
                    "Imported session",
                    1_700_000_000_000_i64,
                    1_700_000_000_100_i64,
                ],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO message VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    "msg_1",
                    "ses_native",
                    1_700_000_000_010_i64,
                    1_700_000_000_020_i64,
                    r#"{"role":"assistant"}"#,
                ],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO part VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    "part_1",
                    "msg_1",
                    "ses_native",
                    1_700_000_000_011_i64,
                    1_700_000_000_011_i64,
                    r#"{"type":"text","text":"opencode"}"#,
                ],
            )
            .unwrap();
        drop(connection);

        let sessions = discover_opencode_sessions(home.path()).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].native_id, "ses_native");
        assert_eq!(sessions[0].cwd.as_deref(), Some(workspace.path()));
        let external = ExternalSession {
            id: external_session_id(ExternalDriver::OpenCode, &sessions[0].native_id),
            revision: sessions[0].revision.clone(),
            driver: ExternalDriver::OpenCode,
            native_id: sessions[0].native_id.clone(),
            transcript: sessions[0].transcript.clone(),
            cwd: sessions[0].cwd.clone(),
            title: sessions[0].title.clone(),
            started_at_unix_ms: sessions[0].started_at_unix_ms,
            updated_at_unix_ms: sessions[0].updated_at_unix_ms,
            process: None,
        };
        let timeline = normalized_timeline(&external).unwrap();
        assert!(
            timeline
                .iter()
                .any(|item| { item["type"] == "content" && item["body"]["text"] == "opencode" })
        );
    }

    #[test]
    fn opencode_timeline_sequences_remain_unique_after_many_tool_parts() {
        let home = tempfile::tempdir().unwrap();
        let parent = home.path().join(".local/share/opencode");
        fs::create_dir_all(&parent).unwrap();
        let database = parent.join("opencode.db");
        let connection = Connection::open(&database).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE message (\
                    id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, \
                    time_updated INTEGER, data TEXT\
                 );\
                 CREATE TABLE part (\
                    id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT, \
                    time_created INTEGER, time_updated INTEGER, data TEXT\
                 );",
            )
            .unwrap();
        for (message_id, created, role) in [
            ("msg_tools", 1_700_000_000_000_i64, "assistant"),
            ("msg_after", 1_700_000_001_000_i64, "user"),
        ] {
            connection
                .execute(
                    "INSERT INTO message VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        message_id,
                        "ses_native",
                        created,
                        created,
                        json!({"role": role}).to_string(),
                    ],
                )
                .unwrap();
        }
        for offset in 0..16_i64 {
            connection
                .execute(
                    "INSERT INTO part VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        format!("part_tool_{offset:02}"),
                        "msg_tools",
                        "ses_native",
                        1_700_000_000_001_i64 + offset,
                        1_700_000_000_001_i64 + offset,
                        json!({
                            "type": "tool",
                            "callID": format!("call_{offset:02}"),
                            "tool": "shell",
                            "state": {
                                "status": "completed",
                                "input": {"command": "true"},
                                "output": "ok"
                            }
                        })
                        .to_string(),
                    ],
                )
                .unwrap();
        }
        connection
            .execute(
                "INSERT INTO part VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    "part_after",
                    "msg_after",
                    "ses_native",
                    1_700_000_001_001_i64,
                    1_700_000_001_001_i64,
                    r#"{"type":"text","text":"after tools"}"#,
                ],
            )
            .unwrap();
        drop(connection);

        let external = ExternalSession {
            id: external_session_id(ExternalDriver::OpenCode, "ses_native"),
            revision: "revision".into(),
            driver: ExternalDriver::OpenCode,
            native_id: "ses_native".into(),
            transcript: database,
            cwd: None,
            title: None,
            started_at_unix_ms: 1_700_000_000_000,
            updated_at_unix_ms: 1_700_000_001_001,
            process: None,
        };
        let timeline = normalized_timeline(&external).unwrap();

        assert_eq!(timeline.len(), 35);
        assert!(timeline.windows(2).all(|pair| {
            pair[0]["sequence"].as_u64().unwrap() < pair[1]["sequence"].as_u64().unwrap()
        }));
        assert_eq!(
            timeline
                .iter()
                .map(|item| item["id"].as_str().unwrap())
                .collect::<BTreeSet<_>>()
                .len(),
            timeline.len()
        );
        let following_message = timeline
            .iter()
            .find(|item| item["body"]["message_id"] == "msg_after")
            .unwrap();
        let final_tool_result = timeline
            .iter()
            .find(|item| item["body"]["call_id"] == "call_15" && item["type"] == "tool_result")
            .unwrap();
        assert!(following_message["sequence"].as_u64() > final_tool_result["sequence"].as_u64());
    }

    #[test]
    fn opencode_timeline_retains_a_bounded_ordered_suffix() {
        let home = tempfile::tempdir().unwrap();
        let database = home.path().join("opencode.db");
        let mut connection = Connection::open(&database).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE message (\
                    id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, \
                    time_updated INTEGER, data TEXT\
                 );\
                 CREATE TABLE part (\
                    id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT, \
                    time_created INTEGER, time_updated INTEGER, data TEXT\
                 );",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO message VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    "msg_many_parts",
                    "ses_native",
                    1_700_000_000_000_i64,
                    1_700_000_000_000_i64,
                    r#"{"role":"assistant"}"#,
                ],
            )
            .unwrap();
        let part_count = MAX_TIMELINE_LINES + 16;
        let transaction = connection.transaction().unwrap();
        {
            let mut insert = transaction
                .prepare("INSERT INTO part VALUES (?1, ?2, ?3, ?4, ?5, ?6)")
                .unwrap();
            for offset in 0..part_count {
                insert
                    .execute(params![
                        format!("part_{offset:05}"),
                        "msg_many_parts",
                        "ses_native",
                        1_700_000_000_001_i64 + offset as i64,
                        1_700_000_000_001_i64 + offset as i64,
                        json!({"type": "text", "text": format!("part {offset}")}).to_string(),
                    ])
                    .unwrap();
            }
        }
        transaction.commit().unwrap();
        drop(connection);

        let external = ExternalSession {
            id: external_session_id(ExternalDriver::OpenCode, "ses_native"),
            revision: "revision".into(),
            driver: ExternalDriver::OpenCode,
            native_id: "ses_native".into(),
            transcript: database,
            cwd: None,
            title: None,
            started_at_unix_ms: 1_700_000_000_000,
            updated_at_unix_ms: 1_700_000_004_112,
            process: None,
        };
        let timeline = normalized_timeline(&external).unwrap();

        assert_eq!(timeline.len(), MAX_TIMELINE_LINES);
        assert_eq!(timeline[0]["type"], "truncation");
        assert!(timeline.windows(2).all(|pair| {
            pair[0]["sequence"].as_u64().unwrap() < pair[1]["sequence"].as_u64().unwrap()
        }));
        assert_eq!(
            timeline.last().unwrap()["body"]["text"],
            format!("part {}", part_count - 1)
        );
        assert!(serde_json::to_vec(&timeline).unwrap().len() <= MAX_TIMELINE_BYTES as usize);
    }

    fn transcript_session(driver: ExternalDriver, path: &Path) -> ExternalSession {
        ExternalSession {
            id: "session/external-test".into(),
            revision: "revision".into(),
            driver,
            native_id: "native".into(),
            transcript: path.to_owned(),
            cwd: None,
            title: None,
            started_at_unix_ms: 0,
            updated_at_unix_ms: 0,
            process: None,
        }
    }

    fn texts(timeline: &[Value]) -> Vec<&str> {
        timeline
            .iter()
            .filter(|item| item["type"] == "content")
            .filter_map(|item| item["body"]["text"].as_str())
            .collect()
    }

    fn assert_unique_ids(timeline: &[Value]) {
        let ids = timeline
            .iter()
            .map(|item| item["id"].as_str().unwrap())
            .collect::<BTreeSet<_>>();
        assert_eq!(ids.len(), timeline.len(), "{timeline:#?}");
    }

    fn claude_line(kind: &str, stamp: &str, text: &str) -> String {
        json!({"type":kind,"timestamp":stamp,"message":{"role":kind,"content":[{"type":"text","text":text}]}})
            .to_string()
    }

    #[test]
    fn a_malformed_line_mid_transcript_costs_only_itself() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("session.jsonl");
        let mut bytes = Vec::new();
        bytes.extend(claude_line("user", "2026-09-30T10:00:00Z", "before").as_bytes());
        bytes.extend(b"\n{\"type\":\"assistant\",\"message\":{\n");
        bytes.extend(b"\xff\xfe not UTF-8\n");
        bytes.extend(claude_line("assistant", "2026-09-30T10:00:01Z", "after").as_bytes());
        bytes.push(b'\n');
        fs::write(&path, bytes).unwrap();

        let timeline =
            normalized_timeline(&transcript_session(ExternalDriver::Claude, &path)).unwrap();

        assert_eq!(texts(&timeline), ["before", "after"]);
        let unreadable = timeline
            .iter()
            .filter(|item| item["body"]["code"] == "native-line-unreadable")
            .collect::<Vec<_>>();
        assert_eq!(unreadable.len(), 2, "{timeline:#?}");
        assert!(
            unreadable
                .iter()
                .all(|item| item["role"] == "system" && item["type"] == "error")
        );
        assert_unique_ids(&timeline);
    }

    #[test]
    fn entries_keep_their_ids_as_the_read_window_slides_along_a_growing_transcript() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("session.jsonl");
        let line = |index: usize| {
            claude_line(
                "assistant",
                &format!("2026-09-30T10:{:02}:{:02}Z", index / 60 % 60, index % 60),
                &format!("entry {index}"),
            )
        };
        let mut transcript = (0..MAX_TIMELINE_LINES)
            .map(|index| format!("{}\n", line(index)))
            .collect::<String>();
        fs::write(&path, &transcript).unwrap();
        let session = transcript_session(ExternalDriver::Claude, &path);
        let before = normalized_timeline(&session).unwrap();
        // Ten more lines push the oldest ten out of the window.
        for index in MAX_TIMELINE_LINES..MAX_TIMELINE_LINES + 10 {
            transcript.push_str(&format!("{}\n", line(index)));
        }
        fs::write(&path, &transcript).unwrap();
        let after = normalized_timeline(&session).unwrap();
        let ids = |timeline: &[Value]| {
            timeline
                .iter()
                .filter_map(|item| {
                    Some((
                        item["body"]["text"].as_str()?.to_owned(),
                        item["id"].clone(),
                    ))
                })
                .collect::<std::collections::HashMap<_, _>>()
        };
        let (before, after) = (ids(&before), ids(&after));
        let kept = before
            .iter()
            .filter(|(text, _)| after.contains_key(*text))
            .collect::<Vec<_>>();
        assert!(kept.len() > MAX_TIMELINE_LINES - 20, "{}", kept.len());
        for (text, id) in kept {
            assert_eq!(&after[text], id, "{text} was renumbered");
        }
        assert_unique_ids(&normalized_timeline(&session).unwrap());
    }

    #[test]
    fn a_record_still_being_written_is_skipped_silently_until_complete() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("session.jsonl");
        let complete = claude_line("assistant", "2026-09-30T10:00:01Z", "partial answer");
        let first = claude_line("user", "2026-09-30T10:00:00Z", "question");
        fs::write(
            &path,
            format!("{first}\n{}", &complete[..complete.len() / 2]),
        )
        .unwrap();
        let session = transcript_session(ExternalDriver::Claude, &path);

        let writing = normalized_timeline(&session).unwrap();
        assert_eq!(texts(&writing), ["question"]);
        assert!(writing.iter().all(|item| item["type"] != "error"));

        fs::write(&path, format!("{first}\n{complete}\n")).unwrap();
        let written = normalized_timeline(&session).unwrap();
        assert_eq!(texts(&written), ["question", "partial answer"]);
        // Entries already seen keep their identity when the record completes.
        assert_eq!(writing[..], written[..writing.len()]);
    }

    #[test]
    fn a_torn_record_keeps_the_whole_record_written_after_it() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("session.jsonl");
        let torn = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"cut off"}],"stop_sequence":n"#;
        let glued = claude_line("user", "2026-09-30T10:00:02Z", "survivor");
        fs::write(
            &path,
            format!(
                "{}\n{torn}{glued}\n",
                claude_line("user", "2026-09-30T10:00:00Z", "first")
            ),
        )
        .unwrap();

        let timeline =
            normalized_timeline(&transcript_session(ExternalDriver::Claude, &path)).unwrap();

        assert_eq!(texts(&timeline), ["first", "survivor"]);
        let notice = timeline
            .iter()
            .find(|item| item["body"]["code"] == "native-line-unreadable")
            .unwrap();
        assert!(notice["body"]["message"].as_str().unwrap().contains("torn"));
        assert_unique_ids(&timeline);
    }

    #[test]
    fn unknown_claude_records_and_blocks_are_labelled_not_dropped_or_attributed() {
        let fallback = timestamp(0);
        let mut items = Vec::new();
        for (sequence, value) in [
            (
                16,
                json!({"type":"brand-new-kind","timestamp":"2026-09-30T10:00:00Z","detail":"x"}),
            ),
            (32, json!({"type":"file-history-snapshot","snapshot":{}})),
            (
                48,
                json!({"timestamp":"2026-09-30T10:00:00Z","note":"no type at all"}),
            ),
            (
                64,
                json!({"type":"assistant","timestamp":"2026-09-30T10:00:01Z","message":{"content":[
                    {"type":"thinking","thinking":"private"},
                    {"type":"hologram","payload":"new"},
                    {"type":"image","source":{"data":"AAAA"}},
                    {"type":"text","text":"visible"}
                ]}}),
            ),
            (
                80,
                json!({"type":"user","timestamp":"2026-09-30T10:00:02Z","message":{"content":[
                    {"type":"tool_result","tool_use_id":"call-1","is_error":true,"content":"boom"}
                ]}}),
            ),
        ] {
            normalize_claude(&value, sequence, &fallback, &mut items);
        }
        let labels = texts(&items);
        assert!(labels[0].starts_with("[unrecognized claude entry `brand-new-kind`]"));
        assert!(labels[1].starts_with("[unrecognized claude entry without a type]"));
        assert!(labels[2].starts_with("[unrecognized claude content block `hologram`]"));
        assert_eq!(labels[3], "[image]");
        assert_eq!(labels[4], "visible");
        assert_eq!(labels.len(), 5);
        assert!(!labels.iter().any(|text| text.contains("private")));
        for item in &items {
            if item["body"]["text"]
                .as_str()
                .is_some_and(|text| text.starts_with("[unrecognized"))
            {
                assert_eq!(item["role"], "system");
            }
        }
        let result = items
            .iter()
            .find(|item| item["type"] == "tool_result")
            .unwrap();
        assert_eq!(result["body"]["status"], "error");
        assert_unique_ids(&items);
    }

    #[test]
    fn an_unrecognized_record_excerpt_is_bounded() {
        let mut items = Vec::new();
        normalize_claude(
            &json!({"type":"brand-new-kind","blob":"é".repeat(MAX_TIMELINE_VALUE_BYTES)}),
            16,
            &timestamp(0),
            &mut items,
        );
        let text = items[0]["body"]["text"].as_str().unwrap();
        assert!(text.len() < MAX_UNRECOGNIZED_BYTES + 128, "{}", text.len());
        assert!(text.ends_with('…'));
    }

    #[test]
    fn claude_queued_prompts_and_system_notes_are_conversation() {
        let fallback = timestamp(0);
        let mut items = Vec::new();
        normalize_claude(
            &json!({"type":"attachment","uuid":"q1","timestamp":"2026-09-30T10:00:00Z","attachment":{"type":"queued_command","prompt":"a message delivered while busy"}}),
            16,
            &fallback,
            &mut items,
        );
        normalize_claude(
            &json!({"type":"attachment","timestamp":"2026-09-30T10:00:00Z","attachment":{"type":"total_tokens_reminder"}}),
            32,
            &fallback,
            &mut items,
        );
        normalize_claude(
            &json!({"type":"system","subtype":"compact_boundary","content":"Conversation compacted","timestamp":"2026-09-30T10:00:01Z"}),
            48,
            &fallback,
            &mut items,
        );
        normalize_claude(
            &json!({"type":"system","subtype":"turn_duration","durationMs":5,"timestamp":"2026-09-30T10:00:01Z"}),
            64,
            &fallback,
            &mut items,
        );
        assert_eq!(
            texts(&items),
            ["a message delivered while busy", "Conversation compacted"]
        );
        assert_eq!(items[1]["role"], "user");
        assert_eq!(items[2]["role"], "system");
    }

    #[test]
    fn an_entry_without_a_usable_timestamp_takes_its_predecessors() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("session.jsonl");
        fs::write(
            &path,
            format!(
                "{}\n{}\n{}\n{}\n",
                claude_line("user", "2026-09-30T10:00:00Z", "stamped"),
                json!({"type":"assistant","message":{"content":"missing"}}),
                json!({"type":"assistant","timestamp":"yesterday","message":{"content":"garbled"}}),
                json!({"type":"assistant","timestamp":1_790_762_401_000_u64,"message":{"content":"numeric"}}),
            ),
        )
        .unwrap();
        let mut session = transcript_session(ExternalDriver::Claude, &path);
        session.updated_at_unix_ms = 1_893_456_000_000;

        let timeline = normalized_timeline(&session).unwrap();
        let stamp = |text: &str| {
            timeline
                .iter()
                .find(|item| item["body"]["text"] == text)
                .unwrap()["timestamp"]
                .as_str()
                .unwrap()
                .to_owned()
        };
        assert_eq!(stamp("missing"), "2026-09-30T10:00:00Z");
        assert_eq!(stamp("garbled"), "2026-09-30T10:00:00Z");
        assert_eq!(stamp("numeric"), "2026-09-30T10:00:01.000Z");
    }

    #[test]
    fn a_record_with_many_parts_does_not_collide_with_the_next_line() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("session.jsonl");
        let parts = (0..40)
            .map(|index| json!({"type":"text","text":format!("part {index}")}))
            .collect::<Vec<_>>();
        fs::write(
            &path,
            format!(
                "{}\n{}\n",
                json!({"type":"assistant","timestamp":"2026-09-30T10:00:00Z","message":{"content":parts}}),
                claude_line("user", "2026-09-30T10:00:01Z", "next line"),
            ),
        )
        .unwrap();
        let timeline =
            normalized_timeline(&transcript_session(ExternalDriver::Claude, &path)).unwrap();
        assert_eq!(texts(&timeline).len(), 41);
        assert_eq!(*texts(&timeline).last().unwrap(), "next line");
        assert_unique_ids(&timeline);
    }

    #[test]
    fn unknown_codex_records_and_items_are_labelled_and_known_ones_stay_hidden() {
        let fallback = timestamp(0);
        let mut items = Vec::new();
        for (sequence, value) in [
            (
                16,
                json!({"type":"event_msg","payload":{"type":"agent_message","message":"dup"}}),
            ),
            (
                32,
                json!({"type":"response_item","payload":{"type":"reasoning","summary":[]}}),
            ),
            (48, json!({"type":"new_record_kind","payload":{}})),
            (
                64,
                json!({"type":"response_item","payload":{"type":"web_search_call","action":{"query":"q"}}}),
            ),
            (
                80,
                json!({"type":"response_item","payload":{"type":"message","role":"user","content":[
                    {"type":"input_image","image_url":"data:"},
                    {"type":"input_hologram"},
                    {"type":"input_text","text":"typed"}
                ]}}),
            ),
        ] {
            normalize_codex(&value, sequence, &fallback, &mut items);
        }
        let labels = texts(&items);
        assert!(labels[0].starts_with("[unrecognized codex record `new_record_kind`]"));
        assert!(labels[1].starts_with("[unrecognized codex response item `web_search_call`]"));
        assert_eq!(labels[2], "[image]");
        assert!(labels[3].starts_with("[unrecognized codex message part `input_hologram`]"));
        assert_eq!(labels[4], "typed");
        assert_eq!(labels.len(), 5);
        assert_unique_ids(&items);
    }

    #[test]
    fn omp_summaries_shell_commands_and_unknown_records_are_visible() {
        let fallback = timestamp(0);
        let mut items = Vec::new();
        for (sequence, value) in [
            (
                16,
                json!({"type":"model_change","id":"a","timestamp":"2026-09-30T10:00:00Z"}),
            ),
            (
                32,
                json!({"type":"compaction","id":"b","timestamp":"2026-09-30T10:00:00Z","summary":"earlier work"}),
            ),
            (
                48,
                json!({"type":"message","id":"c","timestamp":"2026-09-30T10:00:01Z","message":{"role":"bashExecution","command":"ls","output":"file"}}),
            ),
            (64, json!({"type":"future_kind","id":"d"})),
            (
                80,
                json!({"type":"message","id":"e","timestamp":"2026-09-30T10:00:02Z","message":{"role":"assistant","content":[
                    {"type":"thinking","thinking":"private"},
                    {"type":"sparkle"},
                    {"type":"text","text":"answer"}
                ]}}),
            ),
        ] {
            normalize_omp(ExternalDriver::Omp, &value, sequence, &fallback, &mut items);
        }
        let labels = texts(&items);
        assert_eq!(labels[0], "[omp compaction]\nearlier work");
        assert_eq!(labels[1], "$ ls\nfile");
        assert!(labels[2].starts_with("[unrecognized omp entry `future_kind`]"));
        assert!(labels[3].starts_with("[unrecognized omp content block `sparkle`]"));
        assert_eq!(labels[4], "answer");
        assert_eq!(labels.len(), 5);
        let shell = items
            .iter()
            .find(|item| item["body"]["text"] == "$ ls\nfile")
            .unwrap();
        assert_eq!(shell["role"], "system");
    }

    #[test]
    fn opencode_rows_that_cannot_be_decoded_cost_only_themselves() {
        let root = tempfile::tempdir().unwrap();
        let database = root.path().join("opencode.db");
        let connection = Connection::open(&database).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE message (\
                    id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, \
                    time_updated INTEGER, data TEXT\
                 );\
                 CREATE TABLE part (\
                    id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT, \
                    time_created INTEGER, time_updated INTEGER, data TEXT\
                 );\
                 INSERT INTO message VALUES ('msg_1', 'ses', NULL, 1, NULL);\
                 INSERT INTO message VALUES ('msg_2', 'ses', 2, 2, '{\"role\":\"user\"}');\
                 INSERT INTO part VALUES ('p1', 'msg_2', 'ses', 1, 1, NULL);\
                 INSERT INTO part VALUES ('p2', 'msg_2', 'ses', 2, 2, '{\"type\":\"hologram\"}');\
                 INSERT INTO part VALUES ('p3', 'msg_2', 'ses', 3, 3, '{\"type\":\"text\",\"text\":\"kept\"}');\
                 INSERT INTO part VALUES ('p4', 'msg_2', 'ses', 4, 4, '{\"type\":\"file\",\"filename\":\"notes.md\"}');",
            )
            .unwrap();
        drop(connection);
        let mut session = transcript_session(ExternalDriver::OpenCode, &database);
        session.native_id = "ses".into();

        let timeline = normalized_timeline(&session).unwrap();

        let labels = texts(&timeline);
        assert!(labels[0].starts_with("[unrecognized opencode part `hologram`]"));
        assert_eq!(labels[1..], ["kept", "[file: notes.md]"]);
        assert_eq!(
            timeline
                .iter()
                .filter(|item| item["type"] == "message")
                .count(),
            2
        );
        assert!(
            timeline
                .iter()
                .any(|item| item["body"]["code"] == "native-rows-unreadable")
        );
        assert_unique_ids(&timeline);
    }

    #[test]
    fn bound_transcripts_tolerate_naming_and_header_drift() {
        let home = tempfile::tempdir().unwrap();
        // Codex prefixes the thread with its rollout time, and a rollout whose header is
        // missing is still named by its file.
        let thread = "0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b";
        let rollouts = home.path().join(".codex/sessions/2026/09/30");
        fs::create_dir_all(&rollouts).unwrap();
        fs::write(
            rollouts.join(format!("rollout-2026-09-30T10-00-00-{thread}.jsonl")),
            "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\"}}\n",
        )
        .unwrap();
        let codex = find_bound_transcript(home.path(), ExternalDriver::Codex, thread)
            .unwrap()
            .unwrap();
        assert_eq!(codex.native_id, thread);
        // Claude's file name is the session, even when copied history names an earlier one.
        let session = "11111111-2222-4333-8444-555555555555";
        let project = home.path().join(".claude/projects/-work");
        fs::create_dir_all(&project).unwrap();
        fs::write(
            project.join(format!("{session}.jsonl")),
            "{\"sessionId\":\"99999999-2222-4333-8444-555555555555\",\"type\":\"user\"}\n",
        )
        .unwrap();
        let claude = find_bound_transcript(home.path(), ExternalDriver::Claude, session)
            .unwrap()
            .unwrap();
        assert_eq!(claude.native_id, session);
        // A file name that is not a harness session name never becomes a session ID.
        assert_eq!(
            native_id_from_file_name(ExternalDriver::Codex, Path::new("notes.jsonl")),
            None
        );
    }

    #[test]
    fn a_wrapperless_claude_token_names_its_session_and_a_malformed_token_is_refused() {
        let home = tempfile::tempdir().unwrap();
        let session = "11111111-2222-4333-8444-555555555555";
        assert_eq!(
            claude_session_of_managed_driver(
                home.path(),
                "agent/test",
                &format!("{}{session}", st_drivers::harness_state::WRAPPERLESS_PREFIX)
            )
            .unwrap(),
            session
        );
        let refused =
            claude_session_of_managed_driver(home.path(), "agent/test", "provider-current")
                .unwrap_err();
        assert!(
            refused.contains("does not name a driver process"),
            "{refused}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_managed_claude_session_is_proved_only_through_its_own_driver_process() {
        use super::test_support::FakeClaudeDriver;
        let home = tempfile::tempdir().unwrap();
        let session = "11111111-2222-4333-8444-555555555555";
        let fake = FakeClaudeDriver::start("agent/seat");
        let token = fake.token();

        // No session record yet: nothing is guessed.
        let missing =
            claude_session_of_managed_driver(home.path(), "agent/seat", &token).unwrap_err();
        assert!(missing.contains("has recorded its session"), "{missing}");

        // A record whose start time belongs to an earlier process with the same pid is refused.
        fake.record_session(home.path(), session, Some("1"));
        assert!(claude_session_of_managed_driver(home.path(), "agent/seat", &token).is_err());

        fake.record_session(home.path(), session, None);
        assert_eq!(
            claude_session_of_managed_driver(home.path(), "agent/seat", &token).unwrap(),
            session
        );
        // Another seat's evidence cannot claim this driver's Claude.
        let other =
            claude_session_of_managed_driver(home.path(), "agent/other", &token).unwrap_err();
        assert!(other.contains("not this seat's Claude driver"), "{other}");
        // Evidence minted before the process existed names a different, earlier process.
        let early = format!("{}-1000-0", fake.driver.id());
        let reused =
            claude_session_of_managed_driver(home.path(), "agent/seat", &early).unwrap_err();
        assert!(reused.contains("different process"), "{reused}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn takeover_stops_only_the_process_matching_the_exact_start_fingerprint() {
        let bash = path_executable("bash");
        let mut child = std::process::Command::new(&bash)
            .args([
                "-c",
                "exec -a codex \"$1\" -c 'while :; do :; done'",
                "takeover-test",
                bash.to_str().unwrap(),
            ])
            .spawn()
            .unwrap();
        let process = (0..100)
            .find_map(|_| {
                let candidate = platform_processes()
                    .unwrap()
                    .into_iter()
                    .find(|candidate| candidate.process.pid == child.id());
                if candidate.is_none() {
                    std::thread::sleep(Duration::from_millis(10));
                }
                candidate
            })
            .expect("the test codex process should be discoverable")
            .process;

        let mut wrong = process.clone();
        wrong.exact_session = true;
        wrong.fingerprint = "different-start-fingerprint".into();
        assert!(terminate_exact_process(ExternalDriver::Codex, &wrong).is_err());
        assert!(child.try_wait().unwrap().is_none());

        let mut exact = process;
        exact.exact_session = true;
        terminate_exact_process(ExternalDriver::Codex, &exact).unwrap();
        assert!(child.wait().unwrap().code().is_none());
    }
}
