use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::fs::{self, File};
use std::io::{BufRead as _, BufReader, Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::{Arc, Condvar, LazyLock, Mutex, OnceLock};
use std::time::{Duration, Instant};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
#[cfg(not(target_os = "linux"))]
use chrono::NaiveDateTime;
use chrono::{DateTime, Utc};
use kdl::{KdlDocument, KdlEntry, KdlNode};
use rusqlite::{Connection, OpenFlags, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use walkdir::WalkDir;

const MAX_DISCOVERED_FILES: usize = 10_000;
pub(crate) const MAX_EXPOSED_HISTORY: usize = 2_000;
const MAX_METADATA_LINES: usize = 64;
const MAX_TIMELINE_LINES: usize = 4_096;
const MAX_TIMELINE_BYTES: u64 = 32 * 1024 * 1024;
// Wire size limits and owner references are applied by api/client_v0/conversation_blocks.
const DISCOVERY_CACHE_TTL: Duration = Duration::from_secs(2);
/// How long a saved-history request waits for a background transcript read before it answers
/// with the last complete inventory.
const HISTORY_WAIT: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
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
    /// The provider home owning this rollout, independent of the daemon launch environment.
    pub(crate) codex_home: Option<PathBuf>,
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
    codex_home: Option<PathBuf>,
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
            codex_home: item.codex_home,
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
        let mut metadata = metadata;
        if driver == ExternalDriver::Codex {
            metadata.codex_home = Some(home.join(".codex"));
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
        codex_home: metadata.codex_home,
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
        codex_home: metadata.codex_home,
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
            codex_home: metadata.codex_home,
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
/// Volatile entry locator; removed before a timeline leaves the owner. It contains no
/// conversation bytes and is authenticated inside the owner content reference.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "format", rename_all = "kebab-case")]
pub(crate) enum NativeLocator {
    Jsonl {
        offset: u64,
        length: u64,
        digest: String,
        sequence: u64,
        timestamp: String,
        line_index: usize,
    },
    Sqlite {
        part_row: i64,
        message_row: i64,
        digest: String,
        message_digest: String,
        sequence: u64,
        timestamp: String,
        role: String,
    },
    SqliteMessage {
        message_row: i64,
        digest: String,
        sequence: u64,
        timestamp: String,
    },
}

/// Read one identified native record, independent of the size of the session history.
pub(crate) fn normalized_record(
    session: &ExternalSession,
    locator: &NativeLocator,
) -> Result<Vec<Value>> {
    let mut items = Vec::new();
    match locator {
        NativeLocator::Jsonl {
            offset,
            length,
            digest: expected,
            sequence,
            timestamp,
            line_index,
        } => {
            anyhow::ensure!(
                session.driver != ExternalDriver::OpenCode && *length <= MAX_TIMELINE_BYTES,
                "invalid native locator"
            );
            let mut file = File::open(&session.transcript)?;
            file.seek(SeekFrom::Start(*offset))?;
            let mut bytes = Vec::new();
            (&mut file).take(*length).read_to_end(&mut bytes)?;
            anyhow::ensure!(
                bytes.len() as u64 == *length && hex::encode(Sha256::digest(&bytes)) == *expected,
                "native entry changed"
            );
            if bytes.last() != Some(&b'\n') {
                let end = offset.saturating_add(*length);
                let total = file.metadata()?.len();
                if total > end {
                    // A previously incomplete record now has more bytes. A complete JSON
                    // record may acquire its separator without changing the normalized entry.
                    let mut next = [0];
                    file.read_exact(&mut next)?;
                    anyhow::ensure!(
                        next[0] == b'\n' && serde_json::from_slice::<Value>(&bytes).is_ok(),
                        "native tail record changed"
                    );
                }
            }
            normalize_native_bytes(
                session.driver,
                &bytes,
                *sequence,
                timestamp,
                *line_index,
                &mut items,
            );
        }
        NativeLocator::Sqlite {
            part_row,
            message_row,
            digest: expected,
            message_digest,
            sequence,
            timestamp,
            role,
        } => {
            anyhow::ensure!(
                session.driver == ExternalDriver::OpenCode,
                "invalid native locator"
            );
            let connection = open_opencode_database(&session.transcript)?;
            let (message_id, created, encoded) = connection.query_row(
                "SELECT id, time_created, data FROM message WHERE rowid = ?1 AND session_id = ?2",
                params![message_row, session.native_id],
                |row| {
                    sqlite_row_guard(row, &[0, 1, 2])?;
                    Ok((
                        sqlite_bytes(row, 0)?,
                        row.get::<_, Option<i64>>(1).ok().flatten(),
                        sqlite_bytes(row, 2)?,
                    ))
                },
            )?;
            let message = serde_json::from_slice::<Value>(&encoded).unwrap_or(Value::Null);
            anyhow::ensure!(
                native_message_digest(&message_id, created, &message) == *message_digest,
                "native message display context changed"
            );
            let (part_id, encoded) = connection.query_row(
                "SELECT id, data FROM part WHERE rowid = ?1 AND session_id = ?2 AND message_id = (SELECT id FROM message WHERE rowid = ?3 AND session_id = ?2)",
                params![part_row, session.native_id, message_row],
                |row| {
                    sqlite_row_guard(row, &[0, 1])?;
                    Ok((sqlite_bytes(row, 0)?, sqlite_bytes(row, 1)?))
                },
            )?;
            anyhow::ensure!(
                sqlite_part_digest(&part_id, &encoded) == *expected,
                "native part changed"
            );
            let native_role = message.get("role").and_then(Value::as_str);
            let displayed_role = match native_role {
                Some("user" | "assistant" | "system" | "tool" | "toolResult" | "tool_result")
                | None => role.as_str(),
                Some(role) => role,
            };
            normalize_opencode_bytes(
                &encoded,
                &part_id,
                &mut sequence.clone(),
                timestamp,
                displayed_role,
                &mut items,
            )?;
        }
        NativeLocator::SqliteMessage {
            message_row,
            digest: expected,
            sequence,
            timestamp,
        } => {
            anyhow::ensure!(
                session.driver == ExternalDriver::OpenCode,
                "invalid native locator"
            );
            let connection = open_opencode_database(&session.transcript)?;
            let (id, created, encoded) = connection.query_row(
                "SELECT id, time_created, data FROM message WHERE rowid=?1 AND session_id=?2",
                params![message_row, session.native_id],
                |row| {
                    sqlite_row_guard(row, &[0, 1, 2])?;
                    Ok((
                        sqlite_bytes(row, 0)?,
                        sqlite_bytes(row, 1)?,
                        sqlite_bytes(row, 2)?,
                    ))
                },
            )?;
            anyhow::ensure!(
                sqlite_message_digest(&id, &created, &encoded) == *expected,
                "native message changed"
            );
            items.push(opencode_message_record(
                &id, &created, &encoded, *sequence, timestamp,
            ));
        }
    }
    Ok(items)
}

/// Harnesses change their files between releases, crash mid-write, and occasionally tear a
/// record. The reader is liberal in what it accepts: one unreadable unit (a line that is not
/// UTF-8, is not JSON, or carries a kind this reader does not know) costs only that unit, never
/// the rest of the transcript. What it emits stays conservative: an unreadable line or an
/// unrecognized record becomes a clearly-labelled `system` entry, never an entry attributed to
/// the user or the agent. Only opening the file can fail the whole read.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TimelineOrder {
    Sequence,
    TimestampSequence,
}

/// Native entries have rank 0; Small Talk messages have rank 1.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TimelineKey {
    pub(crate) timestamp: String,
    pub(crate) sequence: u64,
    pub(crate) rank: u8,
    pub(crate) id: String,
}

impl TimelineKey {
    pub(crate) fn cmp_in(&self, other: &Self, order: TimelineOrder) -> std::cmp::Ordering {
        compare_timeline_positions(
            (&self.timestamp, self.sequence, self.rank, &self.id),
            (&other.timestamp, other.sequence, other.rank, &other.id),
            order,
        )
    }
}

fn compare_timeline_positions(
    left: (&str, u64, u8, &str),
    right: (&str, u64, u8, &str),
    order: TimelineOrder,
) -> std::cmp::Ordering {
    match order {
        TimelineOrder::Sequence => (left.1, left.2, left.3).cmp(&(right.1, right.2, right.3)),
        TimelineOrder::TimestampSequence => left.cmp(&right),
    }
}

fn native_timeline_position(item: &Value) -> (&str, u64, u8, &str) {
    (
        item["timestamp"].as_str().unwrap_or_default(),
        item["sequence"].as_u64().unwrap_or_default(),
        0,
        item["id"].as_str().unwrap_or_default(),
    )
}

pub(crate) fn timeline_key(item: &Value, rank: u8) -> TimelineKey {
    TimelineKey {
        timestamp: item["timestamp"].as_str().unwrap_or_default().to_owned(),
        sequence: item["sequence"].as_u64().unwrap_or_default(),
        rank,
        id: item["id"].as_str().unwrap_or_default().to_owned(),
    }
}

pub(crate) struct TimelineSlice {
    /// Newest-first entries strictly before the supplied key.
    pub(crate) items: Vec<Value>,
    pub(crate) has_more: bool,
    /// Source content generation: stable across appends, moved by every non-append rebuild.
    pub(crate) generation: u64,
}

#[cfg(test)]
pub(crate) fn timeline_slice(
    session: &ExternalSession,
    order: TimelineOrder,
    before: Option<&TimelineKey>,
    limit: usize,
) -> Result<TimelineSlice> {
    let timeline = LineTimeline::acquire(session);
    let mut held = timeline.hold();
    held.slice(session, order, before, limit)
}

fn collect_timeline_slice<'a>(
    committed: impl Iterator<Item = &'a Value>,
    transient: impl Iterator<Item = &'a Value>,
    order: TimelineOrder,
    before: Option<&TimelineKey>,
    limit: usize,
    clone_output: impl Fn(&Value) -> Value,
) -> TimelineSlice {
    let is_before = |item: &&Value| {
        before.is_none_or(|before| {
            compare_timeline_positions(
                native_timeline_position(item),
                (&before.timestamp, before.sequence, before.rank, &before.id),
                order,
            )
            .is_lt()
        })
    };
    let mut committed = committed.filter(is_before).peekable();
    let mut transient = transient.filter(is_before).peekable();
    let mut items = Vec::new();
    while items.len() < limit {
        let item = match (committed.peek(), transient.peek()) {
            (Some(item), Some(tail))
                if compare_timeline_positions(
                    native_timeline_position(item),
                    native_timeline_position(tail),
                    order,
                )
                .is_lt() =>
            {
                transient.next()
            }
            (Some(_), _) => committed.next(),
            (None, Some(_)) => transient.next(),
            (None, None) => break,
        };
        if let Some(item) = item {
            items.push(clone_output(item));
        }
    }
    TimelineSlice {
        items,
        has_more: committed.peek().is_some() || transient.peek().is_some(),
        generation: 0,
    }
}

const MAX_FOLDED_TRANSCRIPTS: usize = 16;
const MAX_FOLDED_SOURCE_BYTES: usize = 96 * 1024 * 1024;
type SharedLineFold = Arc<Mutex<LineTimelineFold>>;
type LineFoldCache = VecDeque<(PathBuf, ExternalDriver, SharedLineFold, usize)>;
static LINE_FOLDS: LazyLock<Mutex<LineFoldCache>> = LazyLock::new(Mutex::default);

/// Process-random epoch for timeline generations. A restart draws a fresh epoch, so page
/// cursors bound to a generation never carry into another process, matching the cursor MAC
/// key. Within one process, rebuild serials keep generations strictly increasing.
static TIMELINE_GENERATION_EPOCH: LazyLock<u64> = LazyLock::new(|| {
    let mut bytes = [0_u8; 8];
    getrandom::fill(&mut bytes).expect("timeline generation epoch entropy");
    // Clear the top bit so adding rebuild serials cannot wrap in any relevant lifetime.
    u64::from_be_bytes(bytes) & (u64::MAX >> 1)
});
static TIMELINE_REBUILDS: AtomicU64 = AtomicU64::new(0);
fn next_timeline_generation() -> u64 {
    TIMELINE_GENERATION_EPOCH.wrapping_add(
        TIMELINE_REBUILDS
            .fetch_add(1, AtomicOrdering::Relaxed)
            .saturating_add(1),
    )
}

/// Test-only determinism hooks: park one transcript refresh mid-read and observe readers
/// queued on a contended fold. Nothing here is compiled into release builds.
#[cfg(test)]
pub(crate) mod timeline_test_support {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Condvar, LazyLock, Mutex};

    static FOLD_WAITERS: AtomicUsize = AtomicUsize::new(0);
    static REFRESH_GATES: LazyLock<Mutex<HashMap<PathBuf, RefreshGate>>> =
        LazyLock::new(Mutex::default);
    static REFRESH_ARRIVED: Condvar = Condvar::new();

    /// One gated transcript refresh: arrived once its reader parked, released to let it run.
    struct RefreshGate {
        arrived: bool,
        released: bool,
    }

    /// Test hooks never propagate lock poison: a gate recovering its current state is always
    /// sound, matching the fold's own poison recovery.
    fn gates() -> std::sync::MutexGuard<'static, HashMap<PathBuf, RefreshGate>> {
        REFRESH_GATES
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    /// Park the next refresh of `path` until `release_refresh(path)`, so a test controls
    /// exactly when the fold's I/O completes while holding the fold. Gates are per path, so
    /// parallel tests never release each other's readers.
    pub(crate) fn arm_refresh(path: &Path) {
        let mut gates = gates();
        gates.insert(
            path.to_owned(),
            RefreshGate {
                arrived: false,
                released: false,
            },
        );
        REFRESH_ARRIVED.notify_all();
    }

    pub(crate) fn wait_refresh_arrived(path: &Path) {
        let mut gates = gates();
        while !gates.get(path).is_some_and(|gate| gate.arrived) {
            gates = REFRESH_ARRIVED
                .wait(gates)
                .unwrap_or_else(|error| error.into_inner());
        }
    }

    pub(crate) fn release_refresh(path: &Path) {
        let mut gates = gates();
        if let Some(gate) = gates.get_mut(path) {
            gate.released = true;
        }
        REFRESH_ARRIVED.notify_all();
    }

    pub(crate) fn gate_refresh(path: &Path) {
        let mut gates = gates();
        loop {
            let Some(gate) = gates.get_mut(path) else {
                return;
            };
            if gate.released {
                return;
            }
            if !gate.arrived {
                gate.arrived = true;
                REFRESH_ARRIVED.notify_all();
                continue;
            }
            gates = REFRESH_ARRIVED
                .wait(gates)
                .unwrap_or_else(|error| error.into_inner());
        }
    }

    /// Block until `count` readers are parked on a contended fold. They are guaranteed to
    /// arrive: the gated refresh holds that fold for as long as the test wants.
    pub(crate) fn wait_fold_waiters(count: usize) {
        while FOLD_WAITERS.load(Ordering::SeqCst) < count {
            std::thread::yield_now();
        }
    }

    pub(crate) fn enter_contended() {
        FOLD_WAITERS.fetch_add(1, Ordering::SeqCst);
    }

    pub(crate) fn leave_contended() {
        FOLD_WAITERS.fetch_sub(1, Ordering::SeqCst);
    }
}

fn line_fold(session: &ExternalSession) -> SharedLineFold {
    let mut folds = match LINE_FOLDS.lock() {
        Ok(folds) => folds,
        Err(error) => {
            let mut folds = error.into_inner();
            folds.clear();
            LINE_FOLDS.clear_poison();
            folds
        }
    };
    let found = folds
        .iter()
        .position(|(path, driver, _, _)| path == &session.transcript && *driver == session.driver);
    let entry = found
        .and_then(|index| folds.remove(index))
        .unwrap_or_else(|| {
            (
                session.transcript.clone(),
                session.driver,
                SharedLineFold::default(),
                0,
            )
        });
    let fold = Arc::clone(&entry.2);
    folds.push_back(entry);
    trim_line_folds(&mut folds, MAX_FOLDED_SOURCE_BYTES);
    fold
}

fn account_line_fold(fold: &SharedLineFold, bytes: usize) {
    let mut folds = match LINE_FOLDS.lock() {
        Ok(folds) => folds,
        Err(error) => {
            let mut folds = error.into_inner();
            folds.clear();
            LINE_FOLDS.clear_poison();
            folds
        }
    };
    if let Some(index) = folds.iter().position(|entry| Arc::ptr_eq(&entry.2, fold)) {
        let mut entry = folds.remove(index).expect("located fold");
        entry.3 = bytes;
        folds.push_back(entry);
    }
    trim_line_folds(&mut folds, MAX_FOLDED_SOURCE_BYTES);
}

fn trim_line_folds(folds: &mut LineFoldCache, budget: usize) {
    folds.retain(|entry| entry.3 <= budget);
    let mut bytes: usize = folds.iter().map(|entry| entry.3).sum();
    while folds.len() > MAX_FOLDED_TRANSCRIPTS || bytes > budget {
        if let Some(entry) = folds.pop_front() {
            bytes -= entry.3;
        }
    }
}

fn lock_fold(fold: &SharedLineFold) -> std::sync::MutexGuard<'_, LineTimelineFold> {
    if let Ok(state) = fold.try_lock() {
        return state;
    }
    #[cfg(test)]
    timeline_test_support::enter_contended();
    let state = match fold.lock() {
        Ok(state) => state,
        Err(error) => {
            let mut state = error.into_inner();
            *state = LineTimelineFold::default();
            fold.clear_poison();
            state
        }
    };
    #[cfg(test)]
    timeline_test_support::leave_contended();
    state
}

/// One transcript's bounded reader. Whole-source drivers (OpenCode) have no shared fold;
/// line drivers share one, and `hold` waits for it before the caller takes any global read
/// slot, so several viewers of one conversation queue on the fold instead of exhausting the
/// bounded read budget an unrelated conversation still needs.
pub(crate) struct LineTimeline {
    fold: Option<SharedLineFold>,
}

impl LineTimeline {
    /// Resolves the shared fold without locking it and without transcript I/O.
    pub(crate) fn acquire(session: &ExternalSession) -> Self {
        if session.driver == ExternalDriver::OpenCode {
            return Self { fold: None };
        }
        Self {
            fold: Some(line_fold(session)),
        }
    }

    /// Blocks for the fold while holding no caller resource.
    pub(crate) fn hold(&self) -> LineTimelineHeld<'_> {
        LineTimelineHeld {
            fold: self.fold.as_ref(),
            guard: self.fold.as_ref().map(lock_fold),
        }
    }
}

pub(crate) struct LineTimelineHeld<'a> {
    fold: Option<&'a SharedLineFold>,
    guard: Option<std::sync::MutexGuard<'a, LineTimelineFold>>,
}

impl LineTimelineHeld<'_> {
    pub(crate) fn read(&mut self, session: &ExternalSession) -> Result<Vec<Value>> {
        match self.guard.as_mut() {
            Some(state) => {
                let result = state.read(session);
                let fold = self.fold.expect("held fold state has its fold");
                account_line_fold(fold, state.retained_source_bytes);
                result
            }
            None => normalized_opencode_timeline(session),
        }
    }

    pub(crate) fn slice(
        &mut self,
        session: &ExternalSession,
        order: TimelineOrder,
        before: Option<&TimelineKey>,
        limit: usize,
    ) -> Result<TimelineSlice> {
        match self.guard.as_mut() {
            Some(state) => {
                let result = state.slice(session, order, before, limit);
                let fold = self.fold.expect("held fold state has its fold");
                account_line_fold(fold, state.retained_source_bytes);
                result
            }
            None => opencode_timeline_slice(session, order, before, limit),
        }
    }
}

pub(crate) fn normalized_timeline(session: &ExternalSession) -> Result<Vec<Value>> {
    let timeline = LineTimeline::acquire(session);
    let mut held = timeline.hold();
    held.read(session)
}

/// Volatile bounded fold of an append-only JSONL transcript. Replacement, shrinkage and a
/// changed last complete record rebuild it and draw a new generation, expiring page cursors
/// from the replaced content; pure appends keep the generation. Earlier edits are fenced by
/// normalized_record on pinned content reads, rather than by scanning the entire transcript
/// on every page.
#[derive(Default)]
pub(crate) struct LineTimelineFold {
    identity: Option<FileIdentity>,
    last_record: Option<(u64, usize, [u8; 32])>,
    generation: u64,
    consumed: u64,
    observed_len: u64,
    lines: VecDeque<FoldedLine>,
    entries: BTreeMap<(String, u64, u8, String), (usize, usize)>,
    sequence_entries: BTreeMap<(u64, u8, String), (usize, usize)>,
    emitting_lines: BTreeSet<usize>,
    next_line_number: usize,
    seed_timestamp: String,
    retained_source_bytes: usize,
}

struct FoldedLine {
    start: u64,
    /// Stable within this fold; translated to a window-relative index only on output.
    number: usize,
    bytes: Vec<u8>,
    digest: [u8; 32],
    locator: Option<NativeLocator>,
    next_free_sequence: u64,
    last_timestamp: String,
    items: Vec<Value>,
}

impl FoldedLine {
    fn new(start: u64, number: usize, bytes: Vec<u8>) -> Self {
        Self {
            start,
            number,
            digest: Sha256::digest(&bytes).into(),
            bytes,
            locator: None,
            next_free_sequence: 0,
            last_timestamp: String::new(),
            items: Vec::new(),
        }
    }

    fn normalize(
        &mut self,
        driver: ExternalDriver,
        sequence: u64,
        timestamp: &str,
        line_number: usize,
    ) -> Result<()> {
        // Cached locators and diagnostics carry a stable fold ordinal, not an index
        // that needs rewriting whenever the bounded window slides.
        self.items.clear();
        normalize_native_bytes(
            driver,
            &self.bytes,
            sequence,
            timestamp,
            line_number,
            &mut self.items,
        );
        let locator = NativeLocator::Jsonl {
            offset: self.start,
            length: self.bytes.len() as u64,
            digest: hex::encode(self.digest),
            sequence,
            timestamp: timestamp.to_owned(),
            line_index: line_number,
        };
        let source = serde_json::to_value(&locator)?;
        for item in &mut self.items {
            item["_source"] = source.clone();
        }
        self.locator = Some(locator);
        self.next_free_sequence = self
            .items
            .iter()
            .filter_map(|item| item["sequence"].as_u64())
            .max()
            .map_or(sequence, |highest| highest.saturating_add(1));
        self.last_timestamp = self
            .items
            .last()
            .and_then(|item| item["timestamp"].as_str())
            .unwrap_or(timestamp)
            .to_owned();
        Ok(())
    }
}

impl LineTimelineFold {
    fn read(&mut self, session: &ExternalSession) -> Result<Vec<Value>> {
        let mut transient = self.refresh(session)?;
        transient.sort_by_key(|item| item["sequence"].as_u64().unwrap_or(u64::MAX));
        let mut transient = transient.iter().peekable();
        let mut committed = self
            .sequence_entries
            .values()
            .map(|&position| self.indexed_item(position))
            .peekable();
        let mut items = Vec::new();
        // Native sequences are monotonic across records. Merge the small transient
        // set instead of sorting the complete returned window.
        loop {
            let item = match (committed.peek(), transient.peek()) {
                (Some(item), Some(tail))
                    if tail["sequence"].as_u64().unwrap_or(u64::MAX)
                        <= item["sequence"].as_u64().unwrap_or(u64::MAX) =>
                {
                    transient.next()
                }
                (Some(_), _) => committed.next(),
                (None, Some(_)) => transient.next(),
                (None, None) => break,
            };
            if let Some(item) = item {
                items.push(self.clone_output(item));
            }
        }
        Ok(items)
    }

    fn slice(
        &mut self,
        session: &ExternalSession,
        order: TimelineOrder,
        before: Option<&TimelineKey>,
        limit: usize,
    ) -> Result<TimelineSlice> {
        use std::ops::Bound::{Excluded, Unbounded};
        let mut transient = self.refresh(session)?;
        transient.sort_by(|a, b| {
            compare_timeline_positions(
                native_timeline_position(a),
                native_timeline_position(b),
                order,
            )
        });
        let mut slice = match order {
            TimelineOrder::Sequence => {
                let upper = before.map(|key| (key.sequence, key.rank, key.id.clone()));
                let committed = self
                    .sequence_entries
                    .range::<(u64, u8, String), _>((
                        Unbounded,
                        upper.as_ref().map_or(Unbounded, Excluded),
                    ))
                    .rev()
                    .map(|(_, &position)| self.indexed_item(position));
                collect_timeline_slice(
                    committed,
                    transient.iter().rev(),
                    order,
                    before,
                    limit,
                    |item| self.clone_output(item),
                )
            }
            TimelineOrder::TimestampSequence => {
                let upper = before.map(|key| {
                    (
                        key.timestamp.clone(),
                        key.sequence,
                        key.rank,
                        key.id.clone(),
                    )
                });
                let committed = self
                    .entries
                    .range::<(String, u64, u8, String), _>((
                        Unbounded,
                        upper.as_ref().map_or(Unbounded, Excluded),
                    ))
                    .rev()
                    .map(|(_, &position)| self.indexed_item(position));
                collect_timeline_slice(
                    committed,
                    transient.iter().rev(),
                    order,
                    before,
                    limit,
                    |item| self.clone_output(item),
                )
            }
        };
        slice.generation = self.generation;
        Ok(slice)
    }

    fn indexed_item(&self, (number, index): (usize, usize)) -> &Value {
        let first = self
            .lines
            .front()
            .expect("indexed entry has a retained line")
            .number;
        &self.lines[number - first].items[index]
    }

    fn clone_output(&self, item: &Value) -> Value {
        let mut item = item.clone();
        if let Some(number) = item["_source"]["line_index"].as_u64() {
            let first = self
                .lines
                .front()
                .map_or(self.next_line_number, |line| line.number);
            let line_index = number - first as u64;
            item["_source"]["line_index"] = json!(line_index);
            if item["body"]["code"] == "native-line-unreadable" {
                item["body"]["details"]["line_in_window"] = json!(line_index.saturating_add(1));
            }
        }
        item
    }

    fn seed_before(&self, number: usize, fallback: &str) -> (u64, String) {
        self.emitting_lines.range(..number).next_back().map_or_else(
            || (0, fallback.to_owned()),
            |number| {
                let first = self
                    .lines
                    .front()
                    .expect("emitting line is retained")
                    .number;
                let line = &self.lines[*number - first];
                (line.next_free_sequence, line.last_timestamp.clone())
            },
        )
    }

    fn remove_entries(&mut self, items: &[Value]) {
        for item in items {
            let key = timeline_key(item, 0);
            self.sequence_entries
                .remove(&(key.sequence, key.rank, key.id.clone()));
            self.entries
                .remove(&(key.timestamp, key.sequence, key.rank, key.id));
        }
    }

    fn pop_front(&mut self) {
        if let Some(line) = self.lines.pop_front() {
            self.retained_source_bytes -= line.bytes.len();
            self.emitting_lines.remove(&line.number);
            self.remove_entries(&line.items);
        }
    }

    fn refresh(&mut self, session: &ExternalSession) -> Result<Vec<Value>> {
        let mut file = File::open(&session.transcript)
            .with_context(|| format!("read transcript {}", session.transcript.display()))?;
        let metadata = file.metadata()?;
        let len = metadata.len();
        let window_start = len.saturating_sub(MAX_TIMELINE_BYTES);
        let identity = file_identity(&metadata);
        if !self.resumable(&identity, len, window_start, &mut file) {
            // Any non-append change (replacement, shrink, edit, or a cold fold) draws a new
            // generation, so outstanding page cursors stop applying their old boundary.
            *self = Self {
                identity: Some(identity),
                consumed: window_start,
                generation: next_timeline_generation(),
                ..Self::default()
            };
        }
        let first_new_number = self.next_line_number;
        let previous_first = self.lines.front().map(|line| line.number);
        file.seek(SeekFrom::Start(self.consumed))?;
        // Appends beyond this high-water mark belong to the next refresh.
        let mut reader = BufReader::new(file.take(len - self.consumed));
        let mut read_error = None;
        if self.consumed == window_start && window_start != 0 && self.lines.is_empty() {
            let mut partial = Vec::new();
            match reader.read_until(b'\n', &mut partial) {
                Ok(count) => self.consumed += count as u64,
                Err(error) => read_error = Some(error),
            }
        }
        let mut tail = None;
        while read_error.is_none() {
            let mut buffer = Vec::new();
            match reader.read_until(b'\n', &mut buffer) {
                Ok(0) => break,
                Ok(_) if buffer.last() == Some(&b'\n') => {
                    #[cfg(test)]
                    timeline_test_support::gate_refresh(session.transcript.as_path());
                    let line = FoldedLine::new(self.consumed, self.next_line_number, buffer);
                    self.next_line_number += 1;
                    self.consumed += line.bytes.len() as u64;
                    self.last_record = Some((line.start, line.bytes.len(), line.digest));
                    self.retained_source_bytes += line.bytes.len();
                    self.lines.push_back(line);
                    while self.lines.len() > MAX_TIMELINE_LINES {
                        self.pop_front();
                    }
                }
                Ok(_) => {
                    tail = Some(FoldedLine::new(
                        self.consumed,
                        self.next_line_number,
                        buffer,
                    ));
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => read_error = Some(error),
            }
        }
        while window_start != 0
            && self
                .lines
                .front()
                .is_some_and(|line| line.start <= window_start)
        {
            self.pop_front();
        }
        let output_cap = MAX_TIMELINE_LINES - usize::from(tail.is_some());
        while self.lines.len() > output_cap {
            self.pop_front();
        }
        let mut transient = Vec::new();
        if window_start != 0 || self.lines.len() + usize::from(tail.is_some()) == MAX_TIMELINE_LINES
        {
            transient.push(timeline_item(
                0, &timestamp(session.updated_at_unix_ms), "system", "truncation",
                json!({
                    "reason": "the native transcript prefix is outside the bounded read window; not fetchable through this owner read",
                    "fetchable": false,
                    "limit_bytes": MAX_TIMELINE_BYTES,
                    "omitted_from_sequence": 0,
                    "omitted_to_sequence": 0
                }),
            ));
        }
        let seed_timestamp = timestamp(session.started_at_unix_ms);
        let first = self.lines.front().map(|line| line.number);
        let new_index = first.map_or(0, |number| {
            first_new_number
                .saturating_sub(number)
                .min(self.lines.len())
        });
        // Append-only reads begin at the newly added suffix. Eviction or a changed
        // session seed can change inherited timestamps and sequence collisions, so
        // replay only the affected prefix until its cached inputs converge.
        let mut index = if first != previous_first || seed_timestamp != self.seed_timestamp {
            0
        } else {
            new_index
        };
        let (mut next_free_sequence, mut last_timestamp) = if index == 0 {
            (0, seed_timestamp.clone())
        } else {
            self.seed_before(first_new_number, &seed_timestamp)
        };
        while index < self.lines.len() {
            if index < new_index {
                // Omitted records carry neither an output nor a seed transition.
                // Do not replay long runs of them when the initial seed changes.
                index = self
                    .emitting_lines
                    .range(self.lines[index].number..first_new_number)
                    .next()
                    .map_or(new_index, |number| {
                        *number - first.expect("replay has a retained line")
                    });
                if index == self.lines.len() {
                    break;
                }
            }
            let line = &mut self.lines[index];
            let sequence = line
                .start
                .saturating_add(1)
                .saturating_mul(16)
                .max(next_free_sequence);
            let unchanged = matches!(&line.locator, Some(NativeLocator::Jsonl {
                sequence: old_sequence, timestamp: old_timestamp, ..
            }) if *old_sequence == sequence && old_timestamp == &last_timestamp);
            if unchanged && index < new_index {
                // The remaining old suffix has identical inputs and outputs; jump
                // directly to new records, including over runs of omitted records.
                index = new_index;
                (next_free_sequence, last_timestamp) =
                    self.seed_before(first_new_number, &seed_timestamp);
                continue;
            }
            for item in &line.items {
                let key = timeline_key(item, 0);
                self.sequence_entries
                    .remove(&(key.sequence, key.rank, key.id.clone()));
                self.entries
                    .remove(&(key.timestamp, key.sequence, key.rank, key.id));
            }
            line.normalize(session.driver, sequence, &last_timestamp, line.number)?;
            if line.items.is_empty() {
                // An omitted record must not advance the oracle's sequence seed.
                line.next_free_sequence = next_free_sequence;
            }
            if !line.items.is_empty() {
                self.emitting_lines.insert(line.number);
            }
            for (item_index, item) in line.items.iter().enumerate() {
                let key = timeline_key(item, 0);
                self.sequence_entries.insert(
                    (key.sequence, key.rank, key.id.clone()),
                    (line.number, item_index),
                );
                self.entries.insert(
                    (key.timestamp, key.sequence, key.rank, key.id),
                    (line.number, item_index),
                );
            }
            next_free_sequence = line.next_free_sequence;
            last_timestamp.clone_from(&line.last_timestamp);
            index += 1;
        }
        self.seed_timestamp = seed_timestamp;
        if let Some(mut tail) = tail {
            let sequence = tail
                .start
                .saturating_add(1)
                .saturating_mul(16)
                .max(next_free_sequence);
            tail.normalize(session.driver, sequence, &last_timestamp, tail.number)?;
            if !tail.items.is_empty() {
                next_free_sequence = tail.next_free_sequence;
            }
            last_timestamp = tail.last_timestamp;
            transient.extend(tail.items);
        }
        if let Some(error) = read_error {
            transient.push(timeline_item(
                next_free_sequence.max(16), &last_timestamp, "system", "error",
                json!({
                    "code": "native-transcript-read-failed",
                    "message": format!("st stopped reading the {} transcript early: {error}", session.driver.as_str()),
                    "retryable": true,
                    "details": {}
                }),
            ));
        }
        self.observed_len = len;
        Ok(transient)
    }

    fn resumable(
        &self,
        identity: &FileIdentity,
        len: u64,
        window_start: u64,
        file: &mut File,
    ) -> bool {
        if self.identity.as_ref() != Some(identity)
            || len < self.consumed
            || len < self.observed_len
            || self.consumed <= window_start
        {
            return false;
        }
        let Some((start, length, digest)) = self.last_record else {
            return self.consumed == 0;
        };
        let mut record = vec![0; length];
        file.seek(SeekFrom::Start(start)).is_ok()
            && file.read_exact(&mut record).is_ok()
            && <[u8; 32]>::from(Sha256::digest(&record)) == digest
    }
}

#[cfg(unix)]
type FileIdentity = (u64, u64);
#[cfg(not(unix))]
type FileIdentity = Option<SystemTime>;

fn file_identity(metadata: &fs::Metadata) -> FileIdentity {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        (metadata.dev(), metadata.ino())
    }
    #[cfg(not(unix))]
    {
        metadata.created().ok()
    }
}

fn normalize_native_line(
    driver: ExternalDriver,
    value: &Value,
    sequence: u64,
    fallback_timestamp: &str,
    items: &mut Vec<Value>,
) {
    if omission_reason(driver, value).is_some() {
        return;
    }
    let first = items.len();
    match driver {
        ExternalDriver::Codex => normalize_codex(value, sequence, fallback_timestamp, items),
        ExternalDriver::Claude => normalize_claude(value, sequence, fallback_timestamp, items),
        ExternalDriver::Pi | ExternalDriver::Omp => {
            normalize_omp(driver, value, sequence, fallback_timestamp, items)
        }
        // OpenCode history is stored in SQLite and never read line by line.
        ExternalDriver::OpenCode => {}
    }
    if items.len() == first {
        // No parser branch may silently discard a record outside the omission table.
        push_unrecognized(
            items,
            sequence,
            fallback_timestamp,
            driver.as_str(),
            "record",
            value.get("type").and_then(Value::as_str),
            value,
        );
    }
    // Keep the complete native record, including fields a typed projection does not use.
    // This is a display hint, not an authorization or content filter.
    let body = &mut items[first]["body"];
    if body.get("blocks").is_none() {
        body["blocks"] = json!([]);
    }
    body["blocks"].as_array_mut().unwrap().push(json!({
        "id":format!("native-{sequence}/source"), "kind":"source_record",
        "source_type":driver.as_str(), "visibility":"internal", "payload":{"raw":value}
    }));
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

/// Unparseable records preserve exact bytes, including newline, controls and invalid UTF-8.
fn normalize_native_bytes(
    driver: ExternalDriver,
    bytes: &[u8],
    sequence: u64,
    timestamp: &str,
    line_index: usize,
    items: &mut Vec<Value>,
) {
    match serde_json::from_slice::<Value>(bytes) {
        Ok(value) => normalize_native_line(driver, &value, sequence, timestamp, items),
        Err(error) => {
            let recovered = std::str::from_utf8(bytes)
                .ok()
                .and_then(recover_trailing_record);
            let mut item = timeline_item(
                sequence,
                timestamp,
                "system",
                "error",
                json!({
                    "code":"native-line-unreadable",
                    "message":format!("st preserved an unreadable {} transcript record{}", driver.as_str(), if recovered.is_some() { " and recovered the record after the torn prefix" } else { "" }),
                    "retryable":false, "details":{"line_in_window":line_index.saturating_add(1),"parse_error":error.to_string()}
                }),
            );
            item["body"]["blocks"]
                .as_array_mut()
                .unwrap()
                .insert(0, raw_bytes_block(sequence, driver.as_str(), bytes));
            items.push(item);
            if let Some(value) = recovered {
                normalize_native_line(driver, &value, sequence.saturating_add(1), timestamp, items);
            }
        }
    }
}

fn raw_bytes_block(sequence: u64, source_type: &str, bytes: &[u8]) -> Value {
    json!({"id":format!("native-{sequence}/raw"),"kind":"raw_text","source_type":source_type,
        "payload":{"encoding":"base64","bytes":BASE64.encode(bytes),"text":String::from_utf8_lossy(bytes)}})
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
            // Authored `resume` argv bypasses the wrapper's exact-thread handshake.
            // Pin the home as well: another home may contain a stale copy of this ID.
            let home = session
                .codex_home
                .as_deref()
                .context("the saved Codex session has no originating CODEX_HOME")?;
            let home = fs::canonicalize(home)
                .context("resolve the saved Codex session's originating CODEX_HOME")?;
            let mut env = KdlNode::new("env");
            let mut body = KdlDocument::new();
            body.nodes_mut().push(string_node(
                crate::suspension::RESUME_ENV,
                &session.native_id,
            ));
            body.nodes_mut()
                .push(string_node("CODEX_HOME", home.to_string_lossy().as_ref()));
            env.set_children(body);
            agent_body.nodes_mut().push(env);
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
    if !args.entries().is_empty() {
        harness_body.nodes_mut().push(args);
    }
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
                let session_root = roots.iter().find(|(driver, root)| {
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
                if let Some((driver, root)) = session_root
                    && seen.insert(path.to_owned())
                    && found.len() < MAX_DISCOVERED_FILES
                    && let Ok(Some(mut metadata)) = read_metadata(candidate.driver, path)
                {
                    if *driver == ExternalDriver::Codex {
                        metadata.codex_home = root.parent().map(Path::to_owned);
                    }
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
        for entry in WalkDir::new(&root)
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
            if let Ok(Some(mut metadata)) = read_metadata(driver, entry.path()) {
                if driver == ExternalDriver::Codex {
                    metadata.codex_home = root.parent().map(Path::to_owned);
                }
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
            codex_home: None,
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
        codex_home: None,
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

// Streaming updates to token/model/completion metadata do not change an existing part.
// Only message fields that determine its displayed attribution/timing fence the ref.
fn native_message_digest(id: &[u8], created: Option<i64>, message: &Value) -> String {
    digest(&format!(
        "{}:{created:?}:{}",
        BASE64.encode(id),
        message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("system")
    ))
}

const OPENCODE_MESSAGE_WINDOW_SQL: &str = "SELECT rowid, id, time_created, data FROM (\
     SELECT rowid, id, time_created, data FROM message \
     WHERE session_id = ?1 ORDER BY time_created DESC, id DESC LIMIT ?2\
 ) ORDER BY time_created, id";

fn normalized_opencode_timeline(session: &ExternalSession) -> Result<Vec<Value>> {
    Ok(opencode_timeline(session)?.0)
}

fn opencode_timeline_slice(
    session: &ExternalSession,
    order: TimelineOrder,
    before: Option<&TimelineKey>,
    limit: usize,
) -> Result<TimelineSlice> {
    let (mut items, generation) = opencode_timeline(session)?;
    items.sort_by(|a, b| {
        compare_timeline_positions(
            native_timeline_position(a),
            native_timeline_position(b),
            order,
        )
    });
    let mut slice = collect_timeline_slice(
        items.iter().rev(),
        std::iter::empty(),
        order,
        before,
        limit,
        Value::clone,
    );
    slice.generation = generation;
    Ok(slice)
}

fn opencode_timeline(session: &ExternalSession) -> Result<(Vec<Value>, u64)> {
    let connection = open_opencode_database(&session.transcript)?;
    let mut message_statement = connection.prepare(OPENCODE_MESSAGE_WINDOW_SQL)?;
    // Count only row identities, then stream native bytes. Never collect 4,097 payload rows.
    let message_count: usize = connection.query_row(
        "SELECT count(*) FROM (SELECT 1 FROM message WHERE session_id=?1 LIMIT ?2)",
        params![session.native_id, MAX_TIMELINE_LINES as i64 + 1],
        |row| row.get(0),
    )?;
    let mut messages =
        message_statement.query(params![session.native_id, MAX_TIMELINE_LINES as i64 + 1])?;
    let mut truncated = message_count > MAX_TIMELINE_LINES;
    if truncated {
        messages.next()?;
    }
    // A database from an OpenCode release without the part table still shows its messages.
    let mut part_statement = connection
        .prepare(
            "SELECT rowid, id, data FROM part WHERE session_id = ?1 AND message_id = (SELECT id FROM message WHERE rowid = ?2) \
             ORDER BY time_created, id",
        )
        .ok();
    let parts_unavailable = part_statement.is_none();
    let mut items = VecDeque::new();
    // Include the surrounding JSON array delimiters so this remains an exact bound on the
    // serialized timeline, not just on its native payloads. The content generation folds
    // exactly what this read can return, so any database change that alters the bounded
    // timeline moves it.
    let mut generation = Sha256::new();
    let mut serialized_bytes = 2_usize;
    let mut sequence = 1_u64;
    let mut last_created = session.started_at_unix_ms;
    while let Some(row) = messages.next()? {
        let message_row = row.get::<_, i64>(0)?;
        let created = row.get::<_, Option<i64>>(2).ok().flatten();
        if let Some(created) = created {
            last_created = created.max(0) as u128;
        }
        let at = timestamp(last_created);
        let row_bytes = sqlite_row_bytes(row, &[1, 2, 3])?;
        if row_bytes > MAX_TIMELINE_BYTES as usize {
            generation.update(format!("oversized-message:{row_bytes}").as_bytes());
            let notice = oversized_sqlite_row(&mut sequence, &at, "message", row_bytes)?;
            extend_bounded_opencode_timeline(
                &mut items,
                &mut serialized_bytes,
                &mut truncated,
                vec![notice],
            );
            // Its parts have no trustworthy display context until the message fits the bound.
            continue;
        }
        let id_bytes = sqlite_bytes(row, 1)?;
        let created_bytes = sqlite_bytes(row, 2)?;
        let encoded = sqlite_bytes(row, 3)?;
        let message = serde_json::from_slice::<Value>(&encoded).unwrap_or(Value::Null);
        let message_digest = native_message_digest(&id_bytes, created, &message);
        let native_role = message.get("role").and_then(Value::as_str);
        let role = match native_role {
            Some("user" | "assistant" | "system" | "tool" | "toolResult" | "tool_result")
            | None => normalized_role(native_role),
            Some(role) => role,
        };
        let mut additions = Vec::with_capacity(2);
        let entry_sequence = next_opencode_sequence(&mut sequence)?;
        let row_digest = sqlite_message_digest(&id_bytes, &created_bytes, &encoded);
        generation.update(row_digest.as_bytes());
        let mut record =
            opencode_message_record(&id_bytes, &created_bytes, &encoded, entry_sequence, &at);
        record["_source"] = serde_json::to_value(NativeLocator::SqliteMessage {
            message_row,
            digest: row_digest,
            sequence: entry_sequence,
            timestamp: at.clone(),
        })?;
        additions.push(record);
        extend_bounded_opencode_timeline(
            &mut items,
            &mut serialized_bytes,
            &mut truncated,
            additions,
        );
        let Some(part_statement) = part_statement.as_mut() else {
            continue;
        };
        let mut parts = part_statement.query(params![session.native_id, message_row])?;
        while let Some(row) = parts.next()? {
            let part_row = row.get::<_, i64>(0)?;
            let row_bytes = sqlite_row_bytes(row, &[1, 2])?;
            if row_bytes > MAX_TIMELINE_BYTES as usize {
                generation.update(format!("oversized-part:{row_bytes}").as_bytes());
                let notice = oversized_sqlite_row(&mut sequence, &at, "part", row_bytes)?;
                extend_bounded_opencode_timeline(
                    &mut items,
                    &mut serialized_bytes,
                    &mut truncated,
                    vec![notice],
                );
                continue;
            }
            let part_id = sqlite_bytes(row, 1)?;
            let encoded_part = sqlite_bytes(row, 2)?;
            let mut additions = Vec::with_capacity(2);
            let entry_sequence = sequence;
            normalize_opencode_bytes(
                &encoded_part,
                &part_id,
                &mut sequence,
                &at,
                role,
                &mut additions,
            )?;
            let locator = serde_json::to_value(NativeLocator::Sqlite {
                part_row,
                message_row,
                digest: {
                    let part_digest = sqlite_part_digest(&part_id, &encoded_part);
                    generation.update(part_digest.as_bytes());
                    part_digest
                },
                message_digest: message_digest.clone(),
                sequence: entry_sequence,
                timestamp: at.clone(),
                role: normalized_role(Some(role)).to_owned(),
            })?;
            for item in &mut additions {
                item["_source"] = locator.clone();
            }
            extend_bounded_opencode_timeline(
                &mut items,
                &mut serialized_bytes,
                &mut truncated,
                additions,
            );
        }
    }
    if parts_unavailable {
        let message = "st could not read OpenCode's part table, so message contents are missing";
        let notice = timeline_item(
            next_opencode_sequence(&mut sequence)?,
            &timestamp(session.updated_at_unix_ms),
            "system",
            "error",
            json!({
                "code": "native-rows-unreadable",
                "message": message,
                "retryable": false,
                "details": {"parts_unavailable":true}
            }),
        );
        extend_bounded_opencode_timeline(
            &mut items,
            &mut serialized_bytes,
            &mut truncated,
            vec![notice],
        );
    }
    generation.update(
        format!("truncated:{truncated}:count:{message_count}:parts:{parts_unavailable}").as_bytes(),
    );
    let generation = u64::from_be_bytes(
        generation.finalize()[..8]
            .try_into()
            .expect("an 8-byte generation prefix"),
    );
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
                    "reason": "the native OpenCode history prefix is outside the bounded read window; not fetchable through this owner read",
                    "fetchable": false,
                    "limit_bytes": MAX_TIMELINE_BYTES,
                    "omitted_from_sequence": 0,
                    "omitted_to_sequence": 0
                }),
            ),
        );
    }
    Ok((
        items.into_iter().map(|(item, _)| item).collect(),
        generation,
    ))
}

// Inspect SQLite's borrowed cells before allocating any Rust payload copies. Numeric values
// are measured as their small textual representation; text/blob payloads stay borrowed.
fn sqlite_row_bytes(row: &rusqlite::Row<'_>, columns: &[usize]) -> rusqlite::Result<usize> {
    use rusqlite::types::ValueRef;
    columns.iter().try_fold(0_usize, |total, column| {
        let size = match row.get_ref(*column)? {
            ValueRef::Text(bytes) | ValueRef::Blob(bytes) => bytes.len(),
            ValueRef::Null => 4,
            ValueRef::Integer(value) => value.to_string().len(),
            ValueRef::Real(value) => value.to_string().len(),
        };
        Ok(total.saturating_add(size))
    })
}

fn sqlite_row_guard(row: &rusqlite::Row<'_>, columns: &[usize]) -> rusqlite::Result<()> {
    if sqlite_row_bytes(row, columns)? > MAX_TIMELINE_BYTES as usize {
        return Err(rusqlite::Error::FromSqlConversionFailure(
            columns[0],
            row.get_ref(columns[0])?.data_type(),
            Box::new(std::io::Error::other(
                "native SQLite row exceeds the 32 MiB owner-read bound",
            )),
        ));
    }
    Ok(())
}

fn sqlite_bytes(row: &rusqlite::Row<'_>, column: usize) -> rusqlite::Result<Vec<u8>> {
    use rusqlite::types::ValueRef;
    sqlite_row_guard(row, &[column])?;
    Ok(match row.get_ref(column)? {
        ValueRef::Text(bytes) | ValueRef::Blob(bytes) => bytes.to_vec(),
        ValueRef::Null => b"null".to_vec(),
        ValueRef::Integer(value) => value.to_string().into_bytes(),
        ValueRef::Real(value) => value.to_string().into_bytes(),
    })
}

fn oversized_sqlite_row(sequence: &mut u64, at: &str, kind: &str, bytes: usize) -> Result<Value> {
    Ok(timeline_item(
        next_opencode_sequence(sequence)?,
        at,
        "system",
        "error",
        json!({
            "code":"native-record-size-limit",
            "message":format!("not fetchable: OpenCode {kind} row exceeds the 32 MiB owner-read bound ({bytes} source bytes); its contents are unavailable through this read"),
            "retryable":false,
            "details":{"source_type":format!("opencode/{kind}"),"limit_bytes":MAX_TIMELINE_BYTES,"original_bytes":bytes,"fetchable":false,"parts_unavailable":kind == "message"}
        }),
    ))
}

// SQL IDs are part of native identity, even when a rowid survives a rename/rebind.
fn sqlite_part_digest(id: &[u8], encoded: &[u8]) -> String {
    sqlite_message_digest(id, &[], encoded)
}

fn sqlite_message_digest(id: &[u8], created: &[u8], encoded: &[u8]) -> String {
    let mut hash = Sha256::new();
    for bytes in [id, created, encoded] {
        hash.update((bytes.len() as u64).to_le_bytes());
        hash.update(bytes);
    }
    hex::encode(hash.finalize())
}

fn opencode_message_record(
    id: &[u8],
    created: &[u8],
    encoded: &[u8],
    sequence: u64,
    at: &str,
) -> Value {
    let message = serde_json::from_slice::<Value>(encoded);
    let role = message
        .as_ref()
        .ok()
        .and_then(|message| message.get("role"))
        .and_then(Value::as_str);
    let mut items = Vec::new();
    push_message(
        &mut items,
        sequence,
        at,
        normalized_role(role),
        &String::from_utf8_lossy(id),
    );
    let mut block = match message {
        Ok(raw) => {
            json!({"id":format!("native-{sequence}/source"),"kind":"source_record","source_type":"opencode/message","visibility":"internal","payload":{"raw":raw,"id_bytes":BASE64.encode(id),"created_bytes":BASE64.encode(created)}})
        }
        Err(_) => raw_bytes_block(sequence, "opencode/message", encoded),
    };
    block["payload"]["id_bytes"] = json!(BASE64.encode(id));
    block["payload"]["created_bytes"] = json!(BASE64.encode(created));
    items[0]["body"]["blocks"] = json!([block]);
    items.remove(0)
}

fn normalize_opencode_bytes(
    encoded: &[u8],
    id: &[u8],
    sequence: &mut u64,
    at: &str,
    role: &str,
    additions: &mut Vec<Value>,
) -> Result<()> {
    let first = additions.len();
    match serde_json::from_slice::<Value>(encoded) {
        Ok(part) => {
            normalize_opencode_part(&part, sequence, at, role, additions)?;
            if additions.len() == first {
                push_unrecognized(
                    additions,
                    next_opencode_sequence(sequence)?,
                    at,
                    "opencode",
                    "part",
                    part.get("type").and_then(Value::as_str),
                    &part,
                );
            }
            let item = &mut additions[first];
            if item["body"].get("blocks").is_none() {
                item["body"]["blocks"] = json!([]);
            }
            let block_id = format!("{}/source", item["id"].as_str().unwrap());
            item["body"]["blocks"].as_array_mut().unwrap().push(json!({"id":block_id,"kind":"source_record","source_type":"opencode/part","visibility":"internal","payload":{"raw":part,"id_bytes":BASE64.encode(id)}}));
        }
        Err(_) => {
            let seq = next_opencode_sequence(sequence)?;
            let mut item = timeline_item(
                seq,
                at,
                "system",
                "content",
                json!({"media_type":"text/plain","text":format!("[unreadable opencode part]\n{}",String::from_utf8_lossy(encoded))}),
            );
            item["body"]["blocks"] = json!([raw_bytes_block(seq, "opencode/part", encoded)]);
            item["body"]["blocks"][0]["payload"]["id_bytes"] = json!(BASE64.encode(id));
            additions.push(item);
        }
    }
    Ok(())
}

fn normalize_opencode_part(
    part: &Value,
    sequence: &mut u64,
    at: &str,
    role: &str,
    additions: &mut Vec<Value>,
) -> Result<()> {
    if !matches!(role, "user" | "assistant" | "system" | "tool") {
        push_native_block(
            additions,
            next_opencode_sequence(sequence)?,
            at,
            "system",
            (
                "unknown",
                part.get("type").and_then(Value::as_str).unwrap_or("part"),
            ),
            json!({"raw":part,"source_role":role}),
            &format!("[unknown opencode role `{role}`]"),
        );
        return Ok(());
    }
    match part.get("type").and_then(Value::as_str) {
        Some("text") => {
            if let Some(text) = part.get("text").and_then(Value::as_str) {
                push_content(additions, next_opencode_sequence(sequence)?, at, role, text);
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
                additions,
                next_opencode_sequence(sequence)?,
                at,
                call_id,
                name,
                state.get("input").cloned().unwrap_or_else(|| json!({})),
            );
            let status = state.get("status").and_then(Value::as_str);
            if matches!(status, Some("completed" | "error")) {
                push_tool_result_with_status(
                    additions,
                    next_opencode_sequence(sequence)?,
                    at,
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
        Some("image" | "input_image" | "image_url" | "output_image") => {
            push_image(additions, next_opencode_sequence(sequence)?, at, role, part);
        }
        Some(kind) if REASONING_BLOCKS.contains(&kind) => {
            push_reasoning(additions, next_opencode_sequence(sequence)?, at, role, part);
        }
        Some("file")
            if part
                .get("mime")
                .and_then(Value::as_str)
                .is_some_and(|mime| mime.starts_with("image/")) =>
        {
            push_image(additions, next_opencode_sequence(sequence)?, at, role, part);
        }
        Some("file") => {
            let name = part
                .get("filename")
                .or_else(|| part.get("url"))
                .and_then(Value::as_str)
                .unwrap_or("attachment");
            push_content(
                additions,
                next_opencode_sequence(sequence)?,
                at,
                role,
                &format!("[file: {name}]"),
            );
        }
        kind => push_unrecognized(
            additions,
            next_opencode_sequence(sequence)?,
            at,
            "opencode",
            "part",
            kind,
            part,
        ),
    }
    Ok(())
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
    for mut item in additions {
        let raw_size = serde_json::to_vec(&item).map_or(byte_limit, |encoded| encoded.len());
        if raw_size > byte_limit {
            item["_oversized_bytes"] = json!(raw_size);
            let payload_sizes = item["body"]["blocks"]
                .as_array()
                .map(|blocks| {
                    blocks
                        .iter()
                        .map(|block| {
                            let payload = if block["payload"] == json!({"body_ref":true}) {
                                &item["body"]
                            } else {
                                &block["payload"]
                            };
                            serde_json::to_vec(payload).expect("native JSON").len()
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            item["_oversized_payload_bytes"] = json!(payload_sizes);
            let marker = json!(format!(
                "[st truncated this native timeline value: size limit; {raw_size} bytes; fetch from owner]"
            ));
            for key in ["text", "arguments", "content"] {
                if item["body"].get(key).is_some() {
                    item["body"][key] = marker.clone();
                }
            }
            if let Some(blocks) = item["body"]["blocks"].as_array_mut() {
                for block in blocks {
                    if block["payload"] != json!({"body_ref":true}) {
                        block["payload"] = marker.clone();
                    }
                }
            }
        }
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

/// Explicit omission table: only these records may be absent from the data.
/// See docs/st3/conversation-normalization-design.md, native record visibility audit.
fn omission_reason(driver: ExternalDriver, value: &Value) -> Option<&'static str> {
    match (driver, value.get("type").and_then(Value::as_str)) {
        (ExternalDriver::Codex, Some("session_meta")) => Some("setup header, not a chat turn"),
        (ExternalDriver::Codex, Some("response_item"))
            if value.pointer("/payload/type").and_then(Value::as_str) == Some("message")
                && matches!(
                    value.pointer("/payload/role").and_then(Value::as_str),
                    Some("system" | "developer")
                ) =>
        {
            Some("provider bootstrap instructions, not a chat turn")
        }
        (ExternalDriver::Omp | ExternalDriver::Pi, Some("session")) => {
            Some("setup header, not a chat turn")
        }
        (ExternalDriver::Omp | ExternalDriver::Pi, Some("custom_message"))
            if value.get("display") == Some(&Value::Bool(false)) =>
        {
            Some("extension explicitly marks it hidden in the harness")
        }
        _ => None,
    }
}
const REASONING_BLOCKS: &[&str] = &["thinking", "redacted_thinking", "reasoning"];

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

/// Preserve unknown native JSON and label its origin. Transport bounds are applied on reads.
/// Records without an explicit native attribution use the system role.
fn push_unrecognized(
    items: &mut Vec<Value>,
    sequence: u64,
    timestamp: &str,
    driver: &str,
    what: &str,
    kind: Option<&str>,
    value: &Value,
) {
    let block_kind = match kind {
        Some("thinking" | "redacted_thinking" | "reasoning") => {
            push_reasoning(items, sequence, timestamp, "system", value);
            return;
        }
        Some("job" | "job_start" | "job_update" | "job_end") => "job",
        Some("subagent" | "subagent_start" | "subagent_update" | "subagent_end" | "agent") => {
            "subagent"
        }
        Some("ask" | "ask_start" | "ask_update" | "ask_end") => "ask",
        Some(
            "status"
            | "model_change"
            | "thinking_level_change"
            | "step-start"
            | "step-finish"
            | "retry"
            | "compaction"
            | "compacted"
            | "summary",
        ) => "status",
        _ => "unknown",
    };
    let label = match kind {
        Some(kind) => format!("[unrecognized {driver} {what} `{kind}`]"),
        None => format!("[unrecognized {driver} {what} without a type]"),
    };
    let role = value
        .get("role")
        .or_else(|| value.pointer("/message/role"))
        .and_then(Value::as_str)
        .map(|role| normalized_role(Some(role)))
        .unwrap_or("system");
    push_native_block(
        items,
        sequence,
        timestamp,
        role,
        (block_kind, kind.unwrap_or(what)),
        json!({"raw": value}),
        &label,
    );
    if matches!(driver, "omp" | "pi")
        && let Some((kind, view, visibility)) = crate::native_views::omp_record_view(value)
        && let Some(item) = items.last_mut()
    {
        let block = &mut item["body"]["blocks"][0];
        block["kind"] = json!(kind);
        block["view"] = view;
        if let Some(visibility) = visibility {
            block["visibility"] = json!(visibility);
        }
    }
}

/// Native bodies stay owner-local. HTTP negotiation adds size bounds and owner fetch refs;
/// the text is a legacy-client fallback, not a separate capture or storage policy.
fn push_native_block(
    items: &mut Vec<Value>,
    sequence: u64,
    timestamp: &str,
    role: &str,
    (kind, source_type): (&str, &str),
    payload: Value,
    label: &str,
) {
    let fallback = match kind {
        "image" => "[image · load from owner]".to_owned(),
        "reasoning" => payload
            .get("text")
            .and_then(Value::as_str)
            .map(|text| format!("[reasoning]\n{text}"))
            .unwrap_or_else(|| format!("[reasoning]\n{payload}")),
        _ => format!("{label}\n{payload}"),
    };
    let mut item = timeline_item(
        sequence,
        timestamp,
        role,
        "content",
        json!({"media_type":"text/plain", "text":fallback}),
    );
    item["body"]["blocks"] = json!([{
        "id": format!("native-{sequence}/0"), "kind":kind,
        "source_type":source_type, "payload":payload
    }]);
    items.push(item);
}

fn push_reasoning(
    items: &mut Vec<Value>,
    sequence: u64,
    timestamp: &str,
    role: &str,
    part: &Value,
) {
    let text = part
        .get("thinking")
        .or_else(|| part.get("text"))
        .or_else(|| part.get("summary"))
        .cloned()
        .unwrap_or(Value::Null);
    push_native_block(
        items,
        sequence,
        timestamp,
        role,
        (
            "reasoning",
            part.get("type")
                .and_then(Value::as_str)
                .unwrap_or("reasoning"),
        ),
        json!({"text":text,"raw":part}),
        "[reasoning]",
    );
}

fn push_image(items: &mut Vec<Value>, sequence: u64, timestamp: &str, role: &str, part: &Value) {
    push_native_block(
        items,
        sequence,
        timestamp,
        role,
        (
            "image",
            part.get("type").and_then(Value::as_str).unwrap_or("image"),
        ),
        part.clone(),
        "[image]",
    );
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
    if omission_reason(ExternalDriver::Codex, value).is_some() {
        return;
    }
    let timestamp =
        native_timestamp(value.get("timestamp")).unwrap_or_else(|| fallback_timestamp.to_owned());
    match value.get("type").and_then(Value::as_str) {
        Some("response_item") => {}
        kind => {
            push_unrecognized(items, sequence, &timestamp, "codex", "record", kind, value);
            return;
        }
    }
    let payload = &value["payload"];
    match payload.get("type").and_then(Value::as_str) {
        Some("message") => {
            if !matches!(payload["role"].as_str(), Some("user" | "assistant")) {
                push_unrecognized(
                    items,
                    sequence,
                    &timestamp,
                    "codex",
                    "message",
                    payload["role"].as_str(),
                    value,
                );
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
                                push_image(items, part_sequence, &timestamp, role, part)
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
        Some("reasoning") => push_reasoning(items, sequence, &timestamp, "assistant", payload),
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
            } else {
                push_unrecognized(
                    items,
                    sequence,
                    &timestamp,
                    "claude",
                    "attachment",
                    value.pointer("/attachment/type").and_then(Value::as_str),
                    value,
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
            } else {
                push_unrecognized(
                    items,
                    sequence,
                    &timestamp,
                    "claude",
                    "system",
                    value.get("subtype").and_then(Value::as_str),
                    value,
                );
            }
            return;
        }
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
                    Some(kind) if REASONING_BLOCKS.contains(&kind) => {
                        push_reasoning(items, item_sequence, timestamp, role, part);
                    }
                    Some("image") => push_image(items, item_sequence, timestamp, role, part),
                    Some("document") => push_native_block(
                        items,
                        item_sequence,
                        timestamp,
                        role,
                        ("document", "claude/document"),
                        part.clone(),
                        "[document]",
                    ),
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

/// OMP persists a failed assistant turn with `stopReason: "error"`, an `errorMessage`, and —
/// once an automatic retry settles — a `retryRecovery` marker recording how the attempt was
/// recovered or superseded. Project the presentation exactly as the harness resolves it:
/// superseded attempts are hidden, recovered ones render a compact note that still discloses
/// the original error, silent and user-interrupt aborts render nothing, and terminal failures
/// render in full. `label` is the resolved display text; `message` keeps the original error.
fn omp_assistant_error_view(message: &Value) -> Option<Value> {
    const SILENT_ABORT_MARKER: &str = "__omp.silent_abort__";
    const USER_INTERRUPT_LABEL: &str = "Interrupted by user";
    const GENERIC_ABORT_SENTINEL: &str = "Request was aborted";
    let retry = message
        .get("retryRecovery")
        .filter(|retry| retry.is_object());
    let stop_reason = message.get("stopReason").and_then(Value::as_str);
    let error_message = message.get("errorMessage").and_then(Value::as_str);
    let error_id = message.get("errorId").and_then(Value::as_i64);
    let flagged = |bit: i64| error_id.is_some_and(|id| id & bit != 0);
    let silent_abort = error_message == Some(SILENT_ABORT_MARKER) || flagged(0x0200_0000);
    let user_interrupt = error_message == Some(USER_INTERRUPT_LABEL) || flagged(0x0400_0000);
    let retry_note = || {
        retry
            .and_then(|retry| retry.get("note"))
            .and_then(Value::as_str)
            .unwrap_or_default()
    };
    let (status, presentation, is_error, label) = match retry
        .and_then(|retry| retry.get("status"))
        .and_then(Value::as_str)
    {
        Some("recovered") => ("recovered", "compact-recovered", false, retry_note()),
        Some("superseded") => ("superseded", "none", false, ""),
        _ => match stop_reason {
            Some("error") => ("failed", "full", true, error_message.unwrap_or("Error")),
            Some("aborted") => {
                if silent_abort || user_interrupt {
                    return None;
                }
                let custom = error_message.is_some_and(|text| {
                    !text.is_empty()
                        && text != GENERIC_ABORT_SENTINEL
                        && text != SILENT_ABORT_MARKER
                });
                (
                    "failed",
                    "full",
                    true,
                    if custom {
                        error_message.unwrap_or_default()
                    } else {
                        "Operation aborted"
                    },
                )
            }
            _ => {
                let text = error_message?;
                if text.is_empty() || silent_abort || user_interrupt {
                    return None;
                }
                ("failed", "full", true, text)
            }
        },
    };
    let mut view = json!({
        "type": "assistant_error",
        "status": status,
        "presentation": presentation,
        "is_error": is_error,
        "label": label,
        "message": error_message.unwrap_or(""),
    });
    if let Some(stop_reason) = stop_reason {
        view["stop_reason"] = json!(stop_reason);
    }
    if let Some(error_id) = error_id {
        view["error_id"] = json!(error_id);
    }
    for field in ["api", "provider", "model"] {
        if let Some(value) = message.get(field).and_then(Value::as_str) {
            view[field] = json!(value);
        }
    }
    if let Some(retry) = retry {
        let mut projected = json!({});
        for field in ["kind", "status", "recovery", "note"] {
            if let Some(value) = retry.get(field).and_then(Value::as_str) {
                projected[field] = json!(value);
            }
        }
        if let Some(attempt) = retry.get("attempt").and_then(Value::as_u64) {
            projected["attempt"] = json!(attempt);
        }
        if let Some(recovered_at) = retry.get("recoveredAt").and_then(Value::as_str) {
            projected["recovered_at"] = json!(recovered_at);
        }
        if let Some(superseded) = retry.get("supersededBy").filter(|value| value.is_object()) {
            let mut by = json!({});
            if let Some(at) = superseded.get("timestamp").and_then(Value::as_i64) {
                by["timestamp"] = json!(at);
            }
            for (target, field) in [
                ("response_id", "responseId"),
                ("provider", "provider"),
                ("model", "model"),
            ] {
                if let Some(value) = superseded.get(field).and_then(Value::as_str) {
                    by[target] = json!(value);
                }
            }
            if by.as_object().is_some_and(|by| !by.is_empty()) {
                projected["superseded_by"] = by;
            }
        }
        if projected
            .as_object()
            .is_some_and(|projected| !projected.is_empty())
        {
            view["retry"] = projected;
        }
    }
    Some(view)
}

fn normalize_omp(
    driver: ExternalDriver,
    value: &Value,
    sequence: u64,
    fallback_timestamp: &str,
    items: &mut Vec<Value>,
) {
    if omission_reason(driver, value).is_some() {
        return;
    }
    let label = driver.as_str();
    let timestamp = native_timestamp(value.get("timestamp"))
        .or_else(|| native_timestamp(value.pointer("/message/timestamp")))
        .unwrap_or_else(|| fallback_timestamp.to_owned());
    let record_view = crate::native_views::omp_record_view(value);
    let first_item = items.len();
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
            } else {
                push_unrecognized(
                    items,
                    sequence,
                    &timestamp,
                    label,
                    "entry",
                    Some(kind),
                    value,
                );
            }
            if let Some((kind, view, visibility)) = record_view
                && let Some(item) = items.last_mut()
            {
                let block = &mut item["body"]["blocks"][0];
                block["kind"] = json!(kind);
                block["payload"] = json!({"raw":value});
                block["view"] = view;
                if let Some(visibility) = visibility {
                    block["visibility"] = json!(visibility);
                }
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
                if let Some((kind, view, visibility)) = record_view {
                    for item in &mut items[first_item..] {
                        if let Some(blocks) = item["body"]["blocks"].as_array_mut() {
                            for block in blocks {
                                block["kind"] = json!(kind);
                                block["payload"] = json!({"raw":value});
                                block["view"] = view.clone();
                                if let Some(visibility) = visibility {
                                    block["visibility"] = json!(visibility);
                                }
                            }
                        }
                    }
                    if items.len() == first_item {
                        push_unrecognized(
                            items,
                            sequence,
                            &timestamp,
                            label,
                            "entry",
                            Some("custom_message"),
                            value,
                        );
                    }
                }
            }
            return;
        }
        Some("custom") => {
            if let Some((kind, view, visibility)) = record_view
                && view["type"] == "tool_start"
            {
                let tool = view["tool"].as_str().unwrap_or("tool");
                push_native_block(
                    items,
                    sequence,
                    &timestamp,
                    "system",
                    (kind, "custom"),
                    json!({"raw": value}),
                    &format!("[{label} tool start {tool}]"),
                );
                let block = &mut items.last_mut().unwrap()["body"]["blocks"][0];
                block["view"] = view;
                if let Some(visibility) = visibility {
                    block["visibility"] = json!(visibility);
                }
            } else {
                push_unrecognized(
                    items,
                    sequence,
                    &timestamp,
                    label,
                    "entry",
                    Some("custom"),
                    value,
                );
            }
            return;
        }
        Some(record_type @ ("reset_boundary" | "credential_pin" | "title")) => {
            let text = match record_type {
                "reset_boundary" => format!("[{label} session reset]"),
                "credential_pin" => format!(
                    "[{label} credential pin {}]",
                    value["provider"].as_str().unwrap_or_default()
                ),
                _ => format!(
                    "[{label} title] {}",
                    value["title"].as_str().unwrap_or_default()
                ),
            };
            if let Some((kind, view, visibility)) = record_view {
                push_native_block(
                    items,
                    sequence,
                    &timestamp,
                    "system",
                    (kind, record_type),
                    json!({"raw": value}),
                    &text,
                );
                let block = &mut items.last_mut().unwrap()["body"]["blocks"][0];
                block["view"] = view;
                if let Some(visibility) = visibility {
                    block["visibility"] = json!(visibility);
                }
            }
            return;
        }
        kind => {
            push_unrecognized(items, sequence, &timestamp, label, "entry", kind, value);
            return;
        }
    }
    let message = &value["message"];
    let native_role = message.get("role").and_then(Value::as_str);
    if let Some(native_role) = native_role
        && !matches!(
            native_role,
            "user"
                | "assistant"
                | "system"
                | "developer"
                | "tool"
                | "toolResult"
                | "tool_result"
                | "bashExecution"
                | "branchSummary"
                | "compactionSummary"
        )
    {
        push_unrecognized(
            items,
            sequence,
            &timestamp,
            label,
            "message role",
            Some(native_role),
            value,
        );
        return;
    }
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
                message.get("content").cloned().unwrap_or(Value::Null),
                message
                    .get("isError")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            );
            preserve_omp_result_timing(items, message);
            if let Some(item) = items.last_mut() {
                item["body"]["blocks"][0]["view"] = crate::native_views::tool_output_view(
                    message
                        .get("toolName")
                        .and_then(Value::as_str)
                        .unwrap_or("tool"),
                    message["toolCallId"].as_str().unwrap_or_default(),
                    message
                        .get("isError")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    message.get("details"),
                );
                item["body"]["blocks"][0]["payload"] = json!({"body_ref":true,"raw":value});
            }
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
    let first_content = items.len();
    push_omp_content(
        driver,
        items,
        message.get("content").unwrap_or(&Value::Null),
        sequence + 1,
        &timestamp,
        role,
    );
    if native_role == Some("assistant")
        && let Some(view) = omp_assistant_error_view(message)
    {
        // The error presentation is its own block after the turn's content, so every
        // existing native block identity stays stable.
        let error_sequence = items
            .last()
            .and_then(|item| item["sequence"].as_u64())
            .map_or(sequence + 1, |last| last + 1);
        let label = view["label"].as_str().unwrap_or_default().to_owned();
        let fallback = match view["presentation"].as_str() {
            Some("full") => format!("[error] {label}"),
            Some("compact-recovered") => format!("[retry recovered] {label}"),
            _ => String::new(),
        };
        push_content(items, error_sequence, &timestamp, "assistant", &fallback);
        let block = &mut items.last_mut().expect("error item just pushed")["body"]["blocks"][0];
        block["kind"] = json!("error");
        block["source_type"] = json!(format!("{}/assistant_error", driver.as_str()));
        block["payload"] = json!({"body_ref": true});
        block["view"] = view;
    }
    // Message-level metadata has one owner: the last emitted block, including a
    // tool call or an error. Newest-first header windows retain that block even
    // when they split a message, without counting each part as another response.
    if let Some(metadata) = crate::native_views::assistant_metadata(message)
        && let Some(block) = items[first_content..]
            .iter_mut()
            .rev()
            .filter_map(|item| item["body"]["blocks"].as_array_mut())
            .find_map(|blocks| blocks.last_mut())
    {
        if let Some(existing) = block.get_mut("metadata").and_then(Value::as_object_mut) {
            if let Some(additions) = metadata.as_object() {
                existing.extend(
                    additions
                        .iter()
                        .map(|(key, value)| (key.clone(), value.clone())),
                );
            }
        } else {
            block["metadata"] = metadata;
        }
    }
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
                    Some("toolCall" | "tool_call") => {
                        let tool = part.get("name").and_then(Value::as_str).unwrap_or("tool");
                        push_tool_call(
                            items,
                            item_sequence,
                            &timestamp,
                            part.get("id")
                                .or_else(|| part.get("toolCallId"))
                                .and_then(Value::as_str)
                                .unwrap_or("native-call"),
                            tool,
                            part.get("arguments").cloned().unwrap_or_else(|| json!({})),
                        );
                        if let Some(item) = items.last_mut() {
                            item["body"]["blocks"][0]["view"] = crate::native_views::tool_call_view(
                                tool,
                                &item["body"]["arguments"],
                            );
                        }
                    }
                    Some("toolResult" | "tool_result") => {
                        let call_id = part
                            .get("toolCallId")
                            .or_else(|| part.get("call_id"))
                            .and_then(Value::as_str)
                            .unwrap_or("native-call");
                        push_tool_result(
                            items,
                            item_sequence,
                            &timestamp,
                            call_id,
                            part.get("content").cloned().unwrap_or(Value::Null),
                        );
                        if let Some(item) = items.last_mut() {
                            item["body"]["blocks"][0]["view"] =
                                crate::native_views::tool_output_view(
                                    part.get("toolName")
                                        .or_else(|| part.get("name"))
                                        .and_then(Value::as_str)
                                        .unwrap_or("tool"),
                                    call_id,
                                    part.get("isError")
                                        .and_then(Value::as_bool)
                                        .unwrap_or(false),
                                    part.get("details"),
                                );
                            item["body"]["blocks"][0]["payload"] =
                                json!({"body_ref":true,"raw":part});
                        }
                        preserve_omp_result_timing(items, part);
                    }
                    Some(kind) if REASONING_BLOCKS.contains(&kind) => {
                        push_reasoning(items, item_sequence, &timestamp, role, part);
                    }
                    Some("image" | "input_image" | "image_url") => {
                        push_image(items, item_sequence, &timestamp, role, part);
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
    let status = if failed { "error" } else { "success" };
    items.push(timeline_item(sequence, timestamp, "tool", "tool_result", json!({"call_id":call_id, "status":status, "media_type":"application/json", "content":content})));
}

fn preserve_omp_result_timing(items: &mut [Value], native_result: &Value) {
    let Some(details) = native_result.get("details").and_then(Value::as_object) else {
        return;
    };
    if details.is_empty() {
        return;
    }
    let blocks = items.last_mut().expect("result was just appended")["body"]["blocks"]
        .as_array_mut()
        .expect("tool result has normalized blocks");
    for block in blocks {
        if block["kind"] != "tool_output" {
            continue;
        }
        // Keep the native details object open: original names, units and future fields
        // survive beside any metadata already supplied by normalization.
        let metadata = block
            .as_object_mut()
            .expect("normalized block is an object")
            .entry("metadata")
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .expect("block metadata is an object");
        metadata.extend(
            details
                .iter()
                .map(|(key, value)| (key.clone(), value.clone())),
        );
    }
}

fn timeline_item(
    sequence: u64,
    timestamp: &str,
    role: &str,
    entry_type: &str,
    mut body: Value,
) -> Value {
    let kind = match entry_type {
        "content" => Some("text"),
        "tool_call" => Some("tool_call"),
        "tool_result" => Some("tool_output"),
        "status" => Some("status"),
        "error" => Some("error"),
        _ => None,
    };
    if let Some(kind) = kind {
        body["blocks"] = json!([{
            "id":format!("native-{sequence}/0"), "kind":kind,
            "source_type":entry_type, "payload":{"body_ref":true}
        }]);
    }
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

    fn assert_omp_message_usage_once(content: Value) {
        for driver in [ExternalDriver::Omp, ExternalDriver::Pi] {
            let mut items = Vec::new();
            for (sequence, usd) in [(0, 0.25), (1000, 0.5)] {
                let record = json!({
                    "type": "message",
                    "message": {
                        "role": "assistant", "content": content, "model": "test-model",
                        "usage": {"input": 12, "output": 5, "totalTokens": 17, "cost": {"total": usd}},
                        "contextSnapshot": {"promptTokens": 23},
                    },
                });
                let first = items.len();
                normalize_omp(driver, &record, sequence, "2026-10-06T00:00:00Z", &mut items);
                let blocks: Vec<_> = items[first..]
                    .iter()
                    .flat_map(|item| item["body"]["blocks"].as_array().unwrap())
                    .collect();
                let owner = blocks.last().unwrap();
                assert_eq!(owner["metadata"]["usage"]["cost_usd"], usd);
                assert_eq!(owner["metadata"]["usage"]["total"], 17);
                assert_eq!(blocks.iter().filter(|block| block["metadata"]["usage"].is_object()).count(), 1);
            }
            let header = crate::conversation_header::derive(&items, false);
            assert_eq!(header["cost"]["value"]["usd"], 0.75);
            assert_eq!(header["model"]["value"], "test-model");
            assert_eq!(header["context"]["value"]["tokens"], 23);
        }
    }

    #[test]
    fn omp_thinking_and_text_count_message_usage_once() {
        assert_omp_message_usage_once(json!([
            {"type": "thinking", "thinking": "Consider the request"},
            {"type": "text", "text": "Answer"},
        ]));
    }

    #[test]
    fn omp_tool_call_only_counts_message_usage_once() {
        assert_omp_message_usage_once(json!([
            {"type": "toolCall", "id": "first", "name": "read", "arguments": {"path": "a"}},
            {"type": "toolCall", "id": "second", "name": "read", "arguments": {"path": "b"}},
        ]));
    }

    #[test]
    fn omp_split_newest_header_window_keeps_latest_message_metadata() {
        let mut content: Vec<_> = (0..201)
            .map(|index| json!({"type": "text", "text": format!("part {index}")}))
            .collect();
        content.push(json!({
            "type": "toolCall", "id": "last-call", "name": "read", "arguments": {"path": "a"},
        }));
        assert_omp_message_usage_once(json!(content));
        for driver in [ExternalDriver::Omp, ExternalDriver::Pi] {
            let mut items = Vec::new();
            for (sequence, message, timestamp) in [
                (0, json!({
                    "role": "assistant", "content": "previous answer", "model": "old-model",
                    "usage": {"cost": {"total": 0.25}},
                    "contextSnapshot": {"promptTokens": 11},
                }), "2026-10-06T00:00:00Z"),
                (1000, json!({
                    "role": "assistant", "content": content, "model": "latest-model",
                    "usage": {"input": 12, "output": 5, "totalTokens": 17, "cost": {"total": 0.5}},
                    "contextSnapshot": {"promptTokens": 23}, "ttft": 10, "duration": 30,
                }), "2026-10-06T00:00:01Z"),
            ] {
                normalize_omp(driver, &json!({"type": "message", "message": message}),
                    sequence, timestamp, &mut items);
            }
            let owner = &items.last().unwrap()["body"]["blocks"][0];
            assert_eq!(owner["kind"], "tool_call");
            assert_eq!(owner["metadata"]["usage"]["total"], 17);
            assert_eq!(owner["metadata"]["ttft_ms"], 10);
            assert_eq!(owner["metadata"]["duration_ms"], 30);
            // Match the native header's newest-first 200-item window: the latest
            // message spans 202 entries, so its first projected blocks are absent.
            items.reverse();
            let head = &items[..200];
            assert_eq!(head.len(), 200);
            assert!(head.iter().all(|item| item["sequence"].as_u64().unwrap() > 1001));
            let header = crate::conversation_header::derive(head, true);
            assert_eq!(header["cost"]["value"], json!({"usd": 0.5, "window": true}));
            assert_eq!(header["model"]["value"], "latest-model");
            assert_eq!(header["context"]["value"], json!({"tokens": 23, "window": true}));
            for field in ["cost", "model", "context"] {
                assert_eq!(header[field]["as_of"], "2026-10-06T00:00:01Z");
            }
        }
    }

    #[test]
    fn omp_parity_fixture_projects_all_views_without_losing_native_payloads() {
        for driver in [ExternalDriver::Omp, ExternalDriver::Pi] {
            let mut items = Vec::new();
            for (index, line) in include_str!("../fixtures/native-records/omp-parity.jsonl")
                .lines()
                .enumerate()
            {
                let record: Value = serde_json::from_str(line).unwrap();
                let first = items.len();
                normalize_omp(
                    driver,
                    &record,
                    index as u64 * 100,
                    "2026-10-06T00:00:00Z",
                    &mut items,
                );
                if let Some((_, view, _)) = crate::native_views::omp_record_view(&record) {
                    let block = &items[first]["body"]["blocks"][0];
                    assert_eq!(block["payload"]["raw"], record);
                    assert_eq!(block["view"], view);
                }
                if record.pointer("/message/role").and_then(Value::as_str) == Some("toolResult") {
                    assert_eq!(
                        items.last().unwrap()["body"]["content"],
                        record["message"]["content"]
                    );
                    assert_eq!(
                        items.last().unwrap()["body"]["blocks"][0]["payload"]["raw"],
                        record
                    );
                }
                if let Some(parts) = record.pointer("/message/content").and_then(Value::as_array) {
                    for part in parts.iter().filter(|part| part["type"] == "toolCall") {
                        let call = items[first..]
                            .iter()
                            .find(|item| item["type"] == "tool_call")
                            .unwrap();
                        assert_eq!(call["body"]["arguments"], part["arguments"]);
                    }
                }
            }
            let pairs: std::collections::BTreeSet<_> = items
                .iter()
                .flat_map(|item| item["body"]["blocks"].as_array().into_iter().flatten())
                .filter_map(|block| {
                    Some((block["kind"].as_str()?, block["view"]["type"].as_str()?))
                })
                .collect();
            for expected in [
                ("tool_call", "bash"),
                ("tool_call", "edit"),
                ("tool_call", "write"),
                ("tool_call", "read"),
                ("tool_call", "search"),
                ("tool_call", "todo"),
                ("tool_call", "ask"),
                ("tool_call", "task"),
                ("tool_call", "hub"),
                ("tool_call", "eval"),
                ("tool_call", "generic"),
                ("tool_output", "bash"),
                ("tool_output", "edit"),
                ("tool_output", "search"),
                ("tool_output", "todo"),
                ("tool_output", "ask"),
                ("tool_output", "task"),
                ("tool_output", "hub"),
                ("tool_output", "generic"),
                ("irc", "irc"),
                ("job", "job"),
                ("status", "skill"),
                ("status", "compaction"),
                ("status", "model_change"),
                ("status", "thinking_level"),
                ("status", "title"),
                ("status", "session_exit"),
                ("status", "tool_start"),
                ("error", "assistant_error"),
            ] {
                assert!(
                    pairs.contains(&expected),
                    "missing {expected:?} for {driver:?}"
                );
            }
            let blocks: Vec<_> = items
                .iter()
                .flat_map(|item| item["body"]["blocks"].as_array().into_iter().flatten())
                .collect();
            assert!(
                blocks
                    .iter()
                    .any(|block| block["view"]["type"] == "tool_start"
                        && block["visibility"] == "internal")
            );
            assert!(blocks.iter().any(|block| block["metadata"]["context_tokens"] == 23
                && block["metadata"]["usage"]["cost_usd"] == 0.01));
            for kind in ["text", "reasoning"] {
                assert!(blocks.iter().any(|block| block["kind"] == kind));
            }
            assert!(items.iter().any(|item| item["body"]["text"]
                == "Synthetic extension fallback"
                && item["body"]["blocks"][0]["kind"] == "text"));
            assert!(
                !items
                    .iter()
                    .any(|item| item["body"]["text"] == "Synthetic hidden extension")
            );
        }
    }

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
            codex_home: None,
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
            codex_home: None,
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
    fn native_images_keep_original_sources_for_owner_fetch() {
        let part = json!({"type":"image","data":"data:image/png;base64,AAAA","mimeType":"image/png","width":70000});
        for driver in [ExternalDriver::Omp, ExternalDriver::Pi] {
            let mut items = Vec::new();
            normalize_omp(
                driver,
                &json!({"type":"message","id":"image","message":{"role":"user","content":[part.clone()]}}),
                0,
                "",
                &mut items,
            );
            let block = &items[1]["body"]["blocks"][0];
            assert_eq!(block["kind"], "image");
            assert_eq!(block["payload"], part);
            assert_eq!(items[1]["body"]["text"], "[image · load from owner]");
        }
    }

    #[test]
    fn omp_message_tool_result_success_correlates_with_call() {
        let fixture = omp_tool_result_fixture();
        let mut items = Vec::new();
        for (offset, entry) in fixture[..2].iter().enumerate() {
            normalize_omp(
                ExternalDriver::Omp,
                entry,
                offset as u64 * 16,
                "",
                &mut items,
            );
        }
        let call = items
            .iter()
            .find(|item| item["type"] == "tool_call")
            .unwrap();
        let result = items
            .iter()
            .find(|item| item["type"] == "tool_result")
            .unwrap();
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
            normalize_omp(
                ExternalDriver::Omp,
                entry,
                offset as u64 * 16,
                "",
                &mut items,
            );
        }
        let call = items
            .iter()
            .find(|item| item["type"] == "tool_call")
            .unwrap();
        let result = items
            .iter()
            .find(|item| item["type"] == "tool_result")
            .unwrap();
        assert_eq!(result["body"]["call_id"], call["body"]["call_id"]);
        assert_eq!(result["body"]["status"], "error");
        assert_eq!(result["body"]["content"], fixture[3]["message"]["content"]);
    }

    #[test]
    fn pi_and_omp_tool_result_blocks_preserve_native_details_and_source_records() {
        for driver in [ExternalDriver::Omp, ExternalDriver::Pi] {
            for details in [
                json!({"wallTimeMs": 12.75, "timeoutSeconds": 0, "futureTimeNs": 0.125}),
                json!({"wallTimeMs": 0, "timeoutSeconds": 0.125, "future": {"unit": "ticks", "value": 4.5}}),
            ] {
                for nested_kind in [None, Some("toolResult"), Some("tool_result")] {
                    let mut entry = omp_tool_result_fixture()[1].clone();
                    entry["message"]["arguments"] = json!({"timeoutSeconds": 999});
                    if let Some(kind) = nested_kind {
                        entry["message"]
                            .as_object_mut()
                            .unwrap()
                            .remove("toolCallId");
                        entry["message"]["content"] = json!([{
                            "type": kind, "call_id": "nested",
                            "content": "finished", "details": details
                        }]);
                    } else {
                        entry["message"]["details"] = details.clone();
                    }
                    let mut items = Vec::new();
                    normalize_native_line(driver, &entry, 0, "", &mut items);
                    let result = items
                        .iter()
                        .find(|item| item["type"] == "tool_result")
                        .unwrap();
                    assert_eq!(result["body"]["blocks"][0]["kind"], "tool_output");
                    assert_eq!(result["body"]["blocks"][0]["metadata"], details);
                    assert!(result["body"].get("metadata").is_none());
                    let source = items
                        .iter()
                        .flat_map(|item| item["body"]["blocks"].as_array().unwrap())
                        .find(|block| block["kind"] == "source_record")
                        .unwrap();
                    assert_eq!(source["payload"]["raw"], entry);
                    assert!(source.get("metadata").is_none());
                }
            }
        }
    }

    #[test]
    fn native_tool_result_details_merge_without_inventing_absent_timing() {
        let mut entry = omp_tool_result_fixture()[1].clone();
        entry["message"].as_object_mut().unwrap().remove("details");
        entry["message"]["arguments"] = json!({"timeoutSeconds": 999});
        let mut items = Vec::new();
        normalize_native_line(ExternalDriver::Omp, &entry, 0, "", &mut items);
        assert!(
            items.last().unwrap()["body"]["blocks"][0]
                .get("metadata")
                .is_none()
        );
        items.last_mut().unwrap()["body"]["blocks"][0]["metadata"] =
            json!({"existing": true, "wallTimeMs": 999});
        entry["message"]["details"] =
            json!({"wallTimeMs": 0, "timeoutSeconds": 0.125, "future": "unchanged"});
        let source_before = items[0]["body"]["blocks"].clone();
        preserve_omp_result_timing(&mut items, &entry["message"]);
        assert_eq!(
            items.last().unwrap()["body"]["blocks"][0]["metadata"],
            json!({"existing": true, "wallTimeMs": 0, "timeoutSeconds": 0.125, "future": "unchanged"})
        );
        assert_eq!(items[0]["body"]["blocks"], source_before);
    }

    #[test]
    fn omp_failed_assistant_record_appends_typed_error_block() {
        let record = json!({
            "type": "message",
            "id": "err-one",
            "timestamp": "2026-01-01T00:00:00Z",
            "message": {
                "role": "assistant",
                "content": [{"type": "thinking", "thinking": "synthetic planning note"}],
                "api": "openai-codex-responses",
                "provider": "openai-codex",
                "model": "gpt-5.6-sol",
                "stopReason": "error",
                "errorId": 135168,
                "errorMessage": "synthetic socket closed",
                "retryRecovery": {
                    "kind": "auto-retry",
                    "status": "recovered",
                    "attempt": 1,
                    "recoveredAt": "2026-01-01T00:00:20Z",
                    "recovery": "plain",
                    "note": "error; retried",
                    "supersededBy": {
                        "timestamp": 1789031579362i64,
                        "responseId": "resp_synthetic",
                        "provider": "openai-codex",
                        "model": "gpt-5.6-sol"
                    }
                }
            }
        });
        for (driver, source_type) in [
            (ExternalDriver::Omp, "omp/assistant_error"),
            (ExternalDriver::Pi, "pi/assistant_error"),
        ] {
            let mut items = Vec::new();
            normalize_omp(driver, &record, 16, "", &mut items);
            assert_eq!(items.len(), 3);
            // Prior content and its native block identity are untouched.
            assert_eq!(items[1]["type"], "content");
            assert_eq!(items[1]["body"]["blocks"][0]["id"], "native-17/0");
            let error = items.last().unwrap();
            assert_eq!(error["type"], "content");
            assert_eq!(error["role"], "assistant");
            assert_eq!(error["sequence"], 18);
            let block = &error["body"]["blocks"][0];
            assert_eq!(block["id"], "native-18/0");
            assert_eq!(block["kind"], "error");
            assert_eq!(block["source_type"], source_type);
            assert_eq!(block["payload"], json!({"body_ref": true}));
            assert_eq!(block["view"]["type"], "assistant_error");
            assert_eq!(block["view"]["status"], "recovered");
            assert_eq!(block["view"]["presentation"], "compact-recovered");
            assert_eq!(block["view"]["is_error"], false);
            assert_eq!(block["view"]["label"], "error; retried");
            assert_eq!(block["view"]["message"], "synthetic socket closed");
            assert_eq!(block["view"]["stop_reason"], "error");
            assert_eq!(block["view"]["error_id"], 135168);
            assert_eq!(block["view"]["api"], "openai-codex-responses");
            assert_eq!(block["view"]["provider"], "openai-codex");
            assert_eq!(block["view"]["model"], "gpt-5.6-sol");
            assert_eq!(block["view"]["retry"]["kind"], "auto-retry");
            assert_eq!(block["view"]["retry"]["status"], "recovered");
            assert_eq!(block["view"]["retry"]["attempt"], 1);
            assert_eq!(block["view"]["retry"]["recovery"], "plain");
            assert_eq!(block["view"]["retry"]["note"], "error; retried");
            assert_eq!(
                block["view"]["retry"]["recovered_at"],
                "2026-01-01T00:00:20Z"
            );
            assert_eq!(
                block["view"]["retry"]["superseded_by"],
                json!({
                    "timestamp": 1789031579362i64,
                    "response_id": "resp_synthetic",
                    "provider": "openai-codex",
                    "model": "gpt-5.6-sol"
                })
            );
            assert!(
                block["view"]["retry"]["superseded_by"]
                    .get("responseId")
                    .is_none()
            );
            assert_eq!(error["body"]["text"], "[retry recovered] error; retried");
        }
    }

    #[test]
    fn omp_terminal_assistant_error_renders_in_full() {
        let mut record = json!({
            "type": "message",
            "id": "err-two",
            "message": {
                "role": "assistant",
                "content": [],
                "stopReason": "error",
                "errorMessage": "synthetic boom"
            }
        });
        let mut items = Vec::new();
        normalize_omp(ExternalDriver::Omp, &record, 0, "", &mut items);
        let block = &items.last().unwrap()["body"]["blocks"][0];
        assert_eq!(block["view"]["status"], "failed");
        assert_eq!(block["view"]["presentation"], "full");
        assert_eq!(block["view"]["is_error"], true);
        assert_eq!(block["view"]["label"], "synthetic boom");
        assert_eq!(block["view"]["message"], "synthetic boom");
        assert!(block["view"]["retry"].is_null());
        assert!(block["view"]["provider"].is_null());
        assert_eq!(
            items.last().unwrap()["body"]["text"],
            "[error] synthetic boom"
        );

        record["message"]
            .as_object_mut()
            .unwrap()
            .remove("errorMessage");
        let mut items = Vec::new();
        normalize_omp(ExternalDriver::Omp, &record, 0, "", &mut items);
        assert_eq!(
            items.last().unwrap()["body"]["blocks"][0]["view"]["label"],
            "Error"
        );
    }

    #[test]
    fn omp_superseded_retry_error_is_typed_but_not_an_error() {
        let record = json!({
            "type": "message",
            "id": "err-three",
            "message": {
                "role": "assistant",
                "content": [],
                "stopReason": "error",
                "errorMessage": "synthetic rate limited",
                "retryRecovery": {
                    "kind": "auto-retry",
                    "status": "superseded",
                    "attempt": 2,
                    "recovery": "model",
                    "note": "switched model; retried"
                }
            }
        });
        let mut items = Vec::new();
        normalize_omp(ExternalDriver::Omp, &record, 0, "", &mut items);
        let block = &items.last().unwrap()["body"]["blocks"][0];
        assert_eq!(block["kind"], "error");
        assert_eq!(block["view"]["status"], "superseded");
        assert_eq!(block["view"]["presentation"], "none");
        assert_eq!(block["view"]["is_error"], false);
        assert_eq!(block["view"]["label"], "");
        assert_eq!(block["view"]["message"], "synthetic rate limited");
        assert_eq!(block["view"]["retry"]["status"], "superseded");
        assert_eq!(block["view"]["retry"]["attempt"], 2);
        assert!(block["view"]["retry"]["superseded_by"].is_null());
        assert_eq!(items.last().unwrap()["body"]["text"], "");
    }

    #[test]
    fn omp_aborted_assistant_errors_match_native_presentation_rules() {
        let cases = [
            (
                json!({"stopReason":"aborted","errorMessage":"__omp.silent_abort__"}),
                None,
            ),
            (
                json!({"stopReason":"aborted","errorMessage":"Interrupted by user"}),
                None,
            ),
            (
                json!({"stopReason":"aborted","errorId": 0x0400_0000i64, "errorMessage":"synthetic"}),
                None,
            ),
            (
                json!({"stopReason":"aborted","errorId": 0x0200_0000i64}),
                None,
            ),
            (json!({"stopReason":"aborted"}), Some("Operation aborted")),
            (
                json!({"stopReason":"aborted","errorMessage":"Request was aborted"}),
                Some("Operation aborted"),
            ),
            (
                json!({"stopReason":"aborted","errorMessage":"synthetic context limit"}),
                Some("synthetic context limit"),
            ),
            (
                json!({"errorMessage":"synthetic late failure"}),
                Some("synthetic late failure"),
            ),
            (json!({"stopReason":"stop"}), None),
            (json!({}), None),
        ];
        for (fields, expected) in cases {
            let record = json!({
                "type": "message",
                "id": "abort-one",
                "message": {"role": "assistant", "content": []}
            });
            let mut record = record;
            let message = record["message"].as_object_mut().unwrap();
            for (key, value) in fields.as_object().unwrap() {
                message.insert(key.clone(), value.clone());
            }
            let mut items = Vec::new();
            normalize_omp(ExternalDriver::Omp, &record, 0, "", &mut items);
            match expected {
                None => assert_eq!(items.len(), 1, "no error block for {fields}"),
                Some(label) => {
                    let block = &items.last().unwrap()["body"]["blocks"][0];
                    assert_eq!(block["view"]["status"], "failed", "for {fields}");
                    assert_eq!(block["view"]["presentation"], "full", "for {fields}");
                    assert_eq!(block["view"]["is_error"], true, "for {fields}");
                    assert_eq!(block["view"]["label"], label, "for {fields}");
                }
            }
        }
    }

    #[test]
    fn omp_assistant_error_block_keeps_source_record_and_native_identity() {
        let record = json!({
            "type": "message",
            "id": "err-four",
            "message": {
                "role": "assistant",
                "content": "synthetic partial answer",
                "stopReason": "error",
                "errorMessage": "synthetic socket closed"
            }
        });
        let mut items = Vec::new();
        normalize_native_line(ExternalDriver::Omp, &record, 16, "", &mut items);
        // Source record stays on the first item; the error block is separate.
        assert_eq!(items[0]["body"]["blocks"][0]["kind"], "source_record");
        assert_eq!(items[0]["body"]["blocks"][0]["payload"]["raw"], record);
        assert_eq!(items[0]["body"]["blocks"][0]["visibility"], "internal");
        assert_eq!(items[1]["body"]["blocks"][0]["id"], "native-17/0");
        assert_eq!(items[1]["body"]["blocks"][0]["kind"], "text");
        let error = items.last().unwrap();
        assert_eq!(error["sequence"], 18);
        assert_eq!(error["body"]["blocks"][0]["kind"], "error");
        assert_eq!(
            error["body"]["blocks"][0]["source_type"],
            "omp/assistant_error"
        );
        assert_eq!(error["body"]["text"], "[error] synthetic socket closed");
    }

    #[test]
    fn omp_message_tool_result_legacy_block_remains_correlated() {
        let mut entry = omp_tool_result_fixture()[1].clone();
        let message = entry["message"].as_object_mut().unwrap();
        let call_id = message.remove("toolCallId").unwrap();
        let content = message.remove("content").unwrap();
        message.insert(
            "content".to_owned(),
            json!([{
                "type": "toolResult", "toolCallId": call_id, "content": content
            }]),
        );
        let mut items = Vec::new();
        normalize_omp(ExternalDriver::Omp, &entry, 0, "", &mut items);
        let result = items
            .iter()
            .find(|item| item["type"] == "tool_result")
            .unwrap();
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
            codex_home: None,
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
            codex_home: None,
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
            codex_home: None,
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

    #[cfg(unix)]
    #[test]
    fn codex_import_pins_the_physical_home_even_when_discovery_uses_a_symlink() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let origin = home.join(".codex-origin");
        let directory = origin.join("sessions/2026/09/21");
        fs::create_dir_all(&directory).unwrap();
        std::os::unix::fs::symlink(&origin, home.join(".codex")).unwrap();
        let transcript = directory.join("rollout-saved.jsonl");
        fs::write(
            &transcript,
            format!(
                "{}\n",
                json!({
                    "type": "session_meta", "payload": {"id": "saved", "cwd": root.path()}
                })
            ),
        )
        .unwrap();
        let saved = discover_uncached(&home, None, true)
            .unwrap()
            .sessions
            .pop()
            .unwrap();
        let imported = import_seat(&saved).unwrap();
        let origin = fs::canonicalize(origin).unwrap();
        // Repointing the discovery alias after import must never select the stale copy.
        let stale = home.join(".codex-stale");
        fs::create_dir_all(&stale).unwrap();
        fs::remove_file(home.join(".codex")).unwrap();
        std::os::unix::fs::symlink(&stale, home.join(".codex")).unwrap();
        let document: KdlDocument = imported.kdl.parse().unwrap();
        let body = document.get("agent").unwrap().children().unwrap();
        let environment = body.get("env").unwrap().children().unwrap();
        assert_eq!(
            environment
                .get("CODEX_HOME")
                .unwrap()
                .get(0)
                .unwrap()
                .as_string(),
            Some(origin.to_str().unwrap())
        );
        assert_eq!(
            environment
                .get(crate::suspension::RESUME_ENV)
                .unwrap()
                .get(0)
                .unwrap()
                .as_string(),
            Some("saved")
        );
        assert!(
            body.get("harness")
                .unwrap()
                .children()
                .unwrap()
                .get("args")
                .is_none()
        );
    }

    fn transcript_session(driver: ExternalDriver, path: &Path) -> ExternalSession {
        ExternalSession {
            id: "session/external-test".into(),
            revision: "revision".into(),
            driver,
            native_id: "native".into(),
            transcript: path.to_owned(),
            codex_home: None,
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
    fn a_record_still_being_written_is_preserved_until_complete() {
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
        let partial = writing
            .iter()
            .find(|item| item["body"]["code"] == "native-line-unreadable")
            .unwrap();
        assert_eq!(
            BASE64
                .decode(
                    partial["body"]["blocks"][0]["payload"]["bytes"]
                        .as_str()
                        .unwrap()
                )
                .unwrap(),
            &complete.as_bytes()[..complete.len() / 2]
        );
        let locator = serde_json::from_value(partial["_source"].clone()).unwrap();
        assert!(normalized_record(&session, &locator).is_ok());

        fs::write(&path, format!("{first}\n{complete}\n")).unwrap();
        let written = normalized_timeline(&session).unwrap();
        assert_eq!(texts(&written), ["question", "partial answer"]);
        // Entries already seen keep their identity when the record completes.
        assert_eq!(writing[..2], written[..2]);
        assert!(normalized_record(&session, &locator).is_err());
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
        assert!(labels[2].starts_with("[unrecognized claude entry without a type]"));
        assert_eq!(labels[3], "[reasoning]\nprivate");
        assert!(labels[4].starts_with("[unrecognized claude content block `hologram`]"));
        assert_eq!(labels[5], "[image · load from owner]");
        assert_eq!(labels[6], "visible");
        assert_eq!(labels.len(), 7);
        assert!(labels[1].starts_with("[unrecognized claude entry `file-history-snapshot`]"));
        assert!(labels.iter().any(|text| text.contains("private")));
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
    fn omp_developer_reminders_are_system_text_without_unknown_blocks() {
        let text = "<system-reminder>\nSynthetic incomplete todo reminder.\n</system-reminder>";
        let mut record = json!({
            "type": "message",
            "message": {
                "role": "developer",
                "attribution": {"source": "harness"},
                "timestamp": "2026-10-06T12:00:00Z",
                "content": [{"type": "text", "text": text}]
            }
        });
        let mut items = Vec::new();
        normalize_omp(ExternalDriver::Omp, &record, 16, &timestamp(0), &mut items);
        assert!(items.iter().all(|item| item["role"] == "system"));
        let blocks: Vec<_> = items
            .iter()
            .flat_map(|item| item["body"]["blocks"].as_array().into_iter().flatten())
            .collect();
        assert!(blocks.iter().all(|block| block["kind"] != "unknown"));
        assert!(blocks.iter().any(|block| block["kind"] == "text"));
        assert!(items.iter().any(|item| item["body"]["text"] == text));

        // Native attribution is handled exactly as it is for other OMP system messages.
        record["message"]["role"] = json!("system");
        let mut system_items = Vec::new();
        normalize_omp(
            ExternalDriver::Omp,
            &record,
            16,
            &timestamp(0),
            &mut system_items,
        );
        assert_eq!(items, system_items);
    }

    #[test]
    fn future_native_roles_use_legacy_system_envelopes_and_keep_raw_attribution() {
        let stamp = timestamp(0);
        let value = json!({"type":"message","message":{"role":"future-role","content":[{"type":"text","text":"invented-token"}]}});
        let mut items = Vec::new();
        normalize_omp(ExternalDriver::Omp, &value, 16, &stamp, &mut items);
        assert_eq!(items[0]["role"], "system");
        assert_eq!(items[0]["body"]["blocks"][0]["payload"]["raw"], value);
        let mut sequence = 1;
        let mut items = Vec::new();
        let part = json!({"type":"text","text":"invented-token"});
        normalize_opencode_part(&part, &mut sequence, &stamp, "future-role", &mut items).unwrap();
        assert_eq!(items[0]["role"], "system");
        assert_eq!(
            items[0]["body"]["blocks"][0]["payload"],
            json!({"raw":part,"source_role":"future-role"})
        );
        assert_ne!(
            native_message_digest(b"message-one", Some(1), &json!({"role":"future-role"})),
            native_message_digest(b"message-one", Some(1), &json!({"role":"another-role"}))
        );
    }

    #[test]
    fn unknown_json_is_lossless_before_transport_bounding() {
        let value = json!({"type":"brand-new-kind","blob":"é".repeat(8192),"nested":{"token":"invented-token"}});
        let mut items = Vec::new();
        normalize_claude(&value, 16, &timestamp(0), &mut items);
        assert_eq!(items[0]["body"]["blocks"][0]["kind"], "unknown");
        assert_eq!(items[0]["body"]["blocks"][0]["payload"]["raw"], value);
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
        let labels = texts(&items);
        assert_eq!(labels[0], "a message delivered while busy");
        assert!(labels[1].contains("total_tokens_reminder"));
        assert_eq!(labels[2], "Conversation compacted");
        assert!(labels[3].contains("turn_duration"));
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
    fn codex_event_mirrors_and_unknown_records_are_retained_raw() {
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
        assert!(labels[1].starts_with("[reasoning]"));
        assert!(labels[2].starts_with("[unrecognized codex record `new_record_kind`]"));
        assert!(labels[3].starts_with("[unrecognized codex response item `web_search_call`]"));
        assert_eq!(labels[4], "[image · load from owner]");
        assert!(labels[5].starts_with("[unrecognized codex message part `input_hologram`]"));
        assert_eq!(labels[6], "typed");
        assert_eq!(labels.len(), 7);
        assert!(labels[0].contains("event_msg"));
        assert!(labels[0].contains("dup"));
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
        assert!(labels[0].contains("model_change"));
        assert_eq!(labels[1], "[omp compaction]\nearlier work");
        assert_eq!(labels[2], "$ ls\nfile");
        assert!(labels[3].starts_with("[unrecognized omp entry `future_kind`]"));
        assert_eq!(labels[4], "[reasoning]\nprivate");
        assert!(labels[5].starts_with("[unrecognized omp content block `sparkle`]"));
        assert_eq!(labels[6], "answer");
        assert_eq!(labels.len(), 7);
        let shell = items
            .iter()
            .find(|item| item["body"]["text"] == "$ ls\nfile")
            .unwrap();
        assert_eq!(shell["role"], "system");
    }

    #[test]
    fn opencode_rows_preserve_null_unknown_and_malformed_data() {
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
        assert!(labels[0].starts_with("[unrecognized opencode part"));
        assert!(labels[1].starts_with("[unrecognized opencode part `hologram`]"));
        assert_eq!(labels[2..], ["kept", "[file: notes.md]"]);
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
                .all(|item| item["body"]["code"] != "native-rows-unreadable")
        );
        for item in &timeline {
            let locator = serde_json::from_value(item["_source"].clone()).unwrap();
            let fetched = normalized_record(&session, &locator).unwrap();
            assert!(
                fetched
                    .iter()
                    .any(|record| record["id"] == item["id"] && record["body"] == item["body"])
            );
        }
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
    #[test]
    fn every_native_record_is_preserved_unless_the_omission_table_justifies_it() {
        let stamp = timestamp(0);
        let fixtures = [
            (
                ExternalDriver::Codex,
                include_str!("../fixtures/native-records/codex.jsonl"),
            ),
            (
                ExternalDriver::Claude,
                include_str!("../fixtures/native-records/claude.jsonl"),
            ),
            (
                ExternalDriver::Omp,
                include_str!("../fixtures/omp-resume/run4-08-after-retry.jsonl"),
            ),
            (
                ExternalDriver::Pi,
                include_str!("../fixtures/omp-resume/run4-08-after-retry.jsonl"),
            ),
            (
                ExternalDriver::Omp,
                include_str!("../fixtures/omp-tool-results.jsonl"),
            ),
        ];
        for (driver, fixture) in fixtures {
            let mut records = fixture
                .lines()
                .map(|line| serde_json::from_str::<Value>(line).unwrap())
                .collect::<Vec<_>>();
            // Unknown types are deliberate probes, not invented cross-harness known types.
            records.push(json!({"type":"future-kind","raw":"fixture-unknown"}));
            if matches!(driver, ExternalDriver::Omp | ExternalDriver::Pi) {
                records.extend([
                    json!({"type":"custom_message","display":false,"content":"fixture hidden"}),
                    json!({"type":"custom_message","display":true,"content":"fixture visible"}),
                ]);
            }
            for record in records {
                let mut entries = Vec::new();
                normalize_native_line(driver, &record, 16, &stamp, &mut entries);
                assert_eq!(
                    entries.is_empty(),
                    omission_reason(driver, &record).is_some(),
                    "{driver:?} {record}"
                );
                if !entries.is_empty() {
                    assert!(
                        entries
                            .iter()
                            .flat_map(|entry| entry["body"]["blocks"].as_array().unwrap())
                            .any(|block| block["kind"] == "source_record"
                                && block["payload"]["raw"] == record),
                        "{driver:?} {record}"
                    );
                }
            }
        }
        // Real OpenCode capture: test the native row objects, not the SSE envelope.
        let fixture: Value = serde_json::from_str(include_str!(
            "../../st-drivers/tests/fixtures/harness-admission/opencode-1.18.34.json"
        ))
        .unwrap();
        let mut part_types = BTreeSet::new();
        let mut message_count = 0;
        for event in fixture["events"].as_array().unwrap() {
            if event["type"] == "message.updated" {
                let record = &event["properties"]["info"];
                let encoded = serde_json::to_vec(record).unwrap();
                let entry = opencode_message_record(b"fixture-id", b"1", &encoded, 16, &stamp);
                assert!(omission_reason(ExternalDriver::OpenCode, record).is_none());
                assert_eq!(entry["body"]["blocks"][0]["payload"]["raw"], *record);
                message_count += 1;
            } else if event["type"] == "message.part.updated" {
                let record = &event["properties"]["part"];
                part_types.insert(record["type"].as_str().unwrap());
                let mut entries = Vec::new();
                normalize_opencode_bytes(
                    &serde_json::to_vec(record).unwrap(),
                    b"fixture-id",
                    &mut 16,
                    &stamp,
                    "assistant",
                    &mut entries,
                )
                .unwrap();
                assert_eq!(
                    entries.is_empty(),
                    omission_reason(ExternalDriver::OpenCode, record).is_some(),
                    "OpenCode {record}"
                );
                assert!(
                    entries
                        .iter()
                        .flat_map(|entry| entry["body"]["blocks"].as_array().unwrap())
                        .any(|block| block["kind"] == "source_record"
                            && block["payload"]["raw"] == *record)
                );
            }
        }
        assert!(message_count > 0);
        for kind in ["text", "tool", "step-start", "step-finish"] {
            assert!(
                part_types.contains(kind),
                "real capture must exercise {kind}"
            );
        }
        let unknown = json!({"type":"future-part","payload":"fixture"});
        let mut entries = Vec::new();
        normalize_opencode_bytes(
            &serde_json::to_vec(&unknown).unwrap(),
            b"fixture-id",
            &mut 16,
            &stamp,
            "assistant",
            &mut entries,
        )
        .unwrap();
        assert_eq!(entries[0]["body"]["blocks"][0]["kind"], "unknown");
        assert_eq!(entries[0]["body"]["blocks"][0]["payload"]["raw"], unknown);
        assert!(
            omission_reason(ExternalDriver::Codex, &json!({"type":"session_meta"}))
                .unwrap()
                .contains("setup header")
        );
        assert!(
            omission_reason(
                ExternalDriver::Omp,
                &json!({"type":"custom_message","display":false})
            )
            .unwrap()
            .contains("hidden")
        );
    }

    #[test]
    fn malformed_and_unknown_encoding_lines_preserve_exact_bytes_and_direct_reads() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("session.jsonl");
        let bytes = [
            b"not JSON\n".as_slice(),
            b"{\"type\":\"user\",\"text\":\"\xff\"}\n",
            b"\n",
            b"{\"truncated\":",
        ];
        fs::write(&path, bytes.concat()).unwrap();
        let session = transcript_session(ExternalDriver::Claude, &path);
        let entries = normalized_timeline(&session).unwrap();
        assert_eq!(entries.len(), bytes.len());
        for (entry, original) in entries.iter().zip(bytes) {
            let payload = &entry["body"]["blocks"][0]["payload"];
            assert_eq!(
                BASE64.decode(payload["bytes"].as_str().unwrap()).unwrap(),
                original
            );
            let locator = serde_json::from_value(entry["_source"].clone()).unwrap();
            assert_eq!(
                normalized_record(&session, &locator).unwrap()[0]["body"],
                entry["body"]
            );
        }
    }

    #[test]
    fn native_owner_window_jsonl_line_over_32_mib_is_not_fetchable() {
        use std::io::Write as _;

        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("session.jsonl");
        let mut file = std::io::BufWriter::new(File::create(&path).unwrap());
        file.write_all(
            br#"{"type":"message","message":{"role":"user","content":[{"type":"text","text":""#,
        )
        .unwrap();
        std::io::copy(
            &mut std::io::repeat(b'x').take(MAX_TIMELINE_BYTES + 1),
            &mut file,
        )
        .unwrap();
        file.write_all(br#""}]}}"#).unwrap();
        file.flush().unwrap();
        assert!(fs::metadata(&path).unwrap().len() > MAX_TIMELINE_BYTES);

        let session = transcript_session(ExternalDriver::Omp, &path);
        let entries = normalized_timeline(&session).unwrap();
        assert_eq!(entries.len(), 1, "no partial JSON masquerading as a record");
        let notice = &entries[0];
        assert_eq!(notice["type"], "truncation");
        assert_eq!(notice["body"]["fetchable"], false);
        assert_eq!(notice["body"]["limit_bytes"], MAX_TIMELINE_BYTES);
        assert!(
            notice["body"]["reason"]
                .as_str()
                .unwrap()
                .contains("not fetchable")
        );
        assert!(notice.get("_source").is_none(), "no unusable chunk locator");

        // Finish the oversized line and append a complete harness record. The
        // read starts inside the huge line and must resume at its next newline.
        file.write_all(b"\n").unwrap();
        let later = json!({"type":"message","message":{"role":"user","content":[{"type":"text","text":"after the oversized line"}]}});
        serde_json::to_writer(&mut file, &later).unwrap();
        file.write_all(b"\n").unwrap();
        file.flush().unwrap();
        let entries = normalized_timeline(&session).unwrap();
        assert_eq!(entries[0]["body"]["fetchable"], false);
        assert!(texts(&entries).contains(&"after the oversized line"));
        let entry = entries
            .iter()
            .find(|entry| entry["type"] == "content")
            .unwrap();
        let locator = serde_json::from_value(entry["_source"].clone()).unwrap();
        let recovered = normalized_record(&session, &locator)
            .unwrap()
            .into_iter()
            .find(|recovered| recovered["id"] == entry["id"])
            .unwrap();
        assert_eq!(recovered["body"], entry["body"]);
        assert!(serde_json::to_vec(&entries).unwrap().len() < 16 * 1024);
    }

    #[test]
    fn native_owner_window_opencode_query_uses_the_harness_index_before_sorting() {
        let mut db = Connection::open_in_memory().unwrap();
        // OpenCode v1.18.34's native message index, not an index st installs:
        // https://github.com/anomalyco/opencode/blob/aec0b9a6d8898f68f923aaf08b7306d931fd9d76/packages/core/src/session/sql.ts#L64-L76
        db.execute_batch(
            "CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL, time_created INTEGER NOT NULL, data TEXT NOT NULL);\
             CREATE INDEX message_session_time_created_id_idx ON message(session_id, time_created, id);",
        )
        .unwrap();
        let transaction = db.transaction().unwrap();
        {
            let mut insert = transaction
                .prepare("INSERT INTO message VALUES (?1, 'fixture-session', ?2, '{}')")
                .unwrap();
            for index in 0..MAX_TIMELINE_LINES + 3 {
                insert
                    .execute(params![format!("msg-{index:06}"), (index / 2) as i64])
                    .unwrap();
            }
        }
        transaction.execute("INSERT INTO message VALUES ('other-session-message', 'other-session', 999999, '{}')", []).unwrap();
        transaction.commit().unwrap();

        let limit = MAX_TIMELINE_LINES as i64 + 1;
        let mut explain = db
            .prepare(&format!("EXPLAIN QUERY PLAN {OPENCODE_MESSAGE_WINDOW_SQL}"))
            .unwrap();
        let plan = explain
            .query_map(params!["fixture-session", limit], |row| {
                Ok((row.get::<_, i64>(1)?, row.get::<_, String>(3)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        println!("OpenCode bounded message-window plan: {plan:?}");
        assert!(
            plan.iter().any(|(_, detail)| detail
                .contains("SEARCH message USING INDEX message_session_time_created_id_idx")),
            "{plan:?}"
        );
        assert!(
            !plan
                .iter()
                .any(|(_, detail)| detail.starts_with("SCAN message")),
            "{plan:?}"
        );
        for (parent, detail) in &plan {
            if detail.contains("TEMP B-TREE") {
                assert_eq!(
                    *parent, 0,
                    "only the already-limited outer window may sort: {plan:?}"
                );
            }
        }
        let mut messages = db.prepare(OPENCODE_MESSAGE_WINDOW_SQL).unwrap();
        let ids = messages
            .query_map(params!["fixture-session", limit], |row| {
                row.get::<_, String>(1)
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(ids.len(), limit as usize);
        assert_eq!(ids.first().unwrap(), "msg-000002");
        assert_eq!(
            ids.last().unwrap(),
            &format!("msg-{:06}", MAX_TIMELINE_LINES + 2)
        );
        assert!(
            ids.windows(2).all(|pair| pair[0] < pair[1]),
            "timestamp ties keep id ordering"
        );
    }

    #[test]
    fn opencode_oversized_rows_are_explicit_and_do_not_hide_later_rows() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("opencode.db");
        let db = Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, data TEXT); CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT, time_created INTEGER, data TEXT);").unwrap();
        db.execute(
            "INSERT INTO message VALUES ('oversized','ses',1,zeroblob(?1))",
            params![MAX_TIMELINE_BYTES as i64 + 1],
        )
        .unwrap();
        db.execute(
            "INSERT INTO message VALUES ('msg','ses',2,?1)",
            params![r#"{"role":"assistant"}"#],
        )
        .unwrap();
        db.execute(
            "INSERT INTO part VALUES ('oversized','msg','ses',1,zeroblob(?1))",
            params![MAX_TIMELINE_BYTES as i64 + 1],
        )
        .unwrap();
        db.execute(
            "INSERT INTO part VALUES ('part','msg','ses',2,?1)",
            params![r#"{"type":"text","text":"later row is visible"}"#],
        )
        .unwrap();
        let mut session = transcript_session(ExternalDriver::OpenCode, &path);
        session.native_id = "ses".into();
        let entries = normalized_timeline(&session).unwrap();
        let notices = entries
            .iter()
            .filter(|entry| entry["body"]["code"] == "native-record-size-limit")
            .collect::<Vec<_>>();
        assert_eq!(notices.len(), 2);
        for notice in notices {
            assert_eq!(notice["body"]["details"]["fetchable"], false);
            assert!(
                notice["body"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("not fetchable")
            );
            assert!(
                notice.get("_source").is_none(),
                "no unusable continuation ref"
            );
        }
        assert!(texts(&entries).contains(&"later row is visible"));
        db.query_row("SELECT data FROM message WHERE id='oversized'", [], |row| {
            assert!(
                sqlite_bytes(row, 0).is_err(),
                "cell is checked before copying"
            );
            Ok(())
        })
        .unwrap();
        let entry = entries
            .iter()
            .find(|entry| entry["body"]["text"] == "later row is visible")
            .unwrap();
        let locator = serde_json::from_value(entry["_source"].clone()).unwrap();
        db.execute(
            "UPDATE part SET data=zeroblob(?1) WHERE id='part'",
            params![MAX_TIMELINE_BYTES as i64 + 1],
        )
        .unwrap();
        assert!(
            normalized_record(&session, &locator).is_err(),
            "direct ref reads share the row guard"
        );
    }

    #[test]
    fn opencode_invalid_utf8_blob_and_torn_json_keep_their_original_bytes() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("opencode.db");
        let db = Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, data TEXT); CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT, time_created INTEGER, data TEXT);").unwrap();
        let message = b"{\"role\":";
        db.execute(
            "INSERT INTO message VALUES ('msg','ses',1,?1)",
            params![message.as_slice()],
        )
        .unwrap();
        let part = b"bad\xffbytes";
        db.execute(
            "INSERT INTO part VALUES ('part','msg','ses',1,?1)",
            params![part.as_slice()],
        )
        .unwrap();
        let mut session = transcript_session(ExternalDriver::OpenCode, &path);
        session.native_id = "ses".into();
        let entries = normalized_timeline(&session).unwrap();
        assert_eq!(entries.len(), 2);
        for (entry, bytes) in entries.iter().zip([message.as_slice(), part.as_slice()]) {
            assert_eq!(
                BASE64
                    .decode(
                        entry["body"]["blocks"][0]["payload"]["bytes"]
                            .as_str()
                            .unwrap()
                    )
                    .unwrap(),
                bytes
            );
            let locator = serde_json::from_value(entry["_source"].clone()).unwrap();
            assert_eq!(
                normalized_record(&session, &locator).unwrap()[0]["body"],
                entry["body"]
            );
        }
    }
    #[test]
    fn error_blocks_and_source_supplied_timing_fit_the_open_contract() {
        let mut entry = timeline_item(
            16,
            &timestamp(0),
            "system",
            "error",
            json!({"code":"native-stop","message":"provider stopped","retryable":true,"details":{"stopReason":"future","errorMessage":"invented-token"}}),
        );
        assert_eq!(entry["body"]["blocks"][0]["kind"], "error");
        assert_eq!(
            entry["body"]["blocks"][0]["payload"],
            json!({"body_ref":true})
        );
        let metadata =
            json!({"wallTimeMs":12.75,"timeoutSeconds":0,"future":{"value":"untouched"}});
        entry["body"]["blocks"][0]["metadata"] = metadata.clone();
        let decoded: st3_client::TimelineEntry = serde_json::from_value(entry).unwrap();
        let st3_client::TimelineBody::Error(body) = decoded.body else {
            panic!("known error envelope")
        };
        assert_eq!(body.blocks[0].metadata.as_ref(), Some(&metadata));
    }

    // Claude has no omitted record kinds: these fillers remain visible system entries.
    const CLAUDE_WINDOW_FILLER: &str = "{\"type\":\"file-history-snapshot\"}\n";

    fn chat_texts(timeline: &[Value]) -> Vec<&str> {
        timeline
            .iter()
            .filter(|item| item["type"] == "content")
            .filter(|item| matches!(item["role"].as_str(), Some("user" | "assistant")))
            .filter_map(|item| item["body"]["text"].as_str())
            .collect()
    }

    /// Independent bounded-reader oracle, retaining the upstream normalization inputs.
    fn unfurled_timeline(session: &ExternalSession) -> Vec<Value> {
        let bytes = fs::read(&session.transcript).unwrap();
        let start = bytes.len().saturating_sub(MAX_TIMELINE_BYTES as usize);
        let mut offset = start;
        if start != 0 {
            offset += bytes[start..]
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(bytes.len() - start, |index| index + 1);
        }
        let mut lines = VecDeque::new();
        for line in bytes[offset..].split_inclusive(|byte| *byte == b'\n') {
            if lines.len() == MAX_TIMELINE_LINES {
                lines.pop_front();
            }
            lines.push_back((offset as u64, line));
            offset += line.len();
        }
        let mut items = Vec::new();
        if start != 0 || lines.len() == MAX_TIMELINE_LINES {
            items.push(timeline_item(
                0, &timestamp(session.updated_at_unix_ms), "system", "truncation",
                json!({
                    "reason": "the native transcript prefix is outside the bounded read window; not fetchable through this owner read",
                    "fetchable": false, "limit_bytes": MAX_TIMELINE_BYTES,
                    "omitted_from_sequence": 0, "omitted_to_sequence": 0
                }),
            ));
        }
        let mut last_timestamp = timestamp(session.started_at_unix_ms);
        let mut next_free_sequence = 0;
        for (line_index, (offset, bytes)) in lines.into_iter().enumerate() {
            let sequence = offset
                .saturating_add(1)
                .saturating_mul(16)
                .max(next_free_sequence);
            let first = items.len();
            normalize_native_bytes(
                session.driver,
                bytes,
                sequence,
                &last_timestamp,
                line_index,
                &mut items,
            );
            let source = serde_json::to_value(NativeLocator::Jsonl {
                offset,
                length: bytes.len() as u64,
                digest: hex::encode(Sha256::digest(bytes)),
                sequence,
                timestamp: last_timestamp.clone(),
                line_index,
            })
            .unwrap();
            for item in &mut items[first..] {
                item["_source"] = source.clone();
            }
            if let Some(highest) = items[first..]
                .iter()
                .filter_map(|item| item["sequence"].as_u64())
                .max()
            {
                next_free_sequence = highest.saturating_add(1);
            }
            if let Some(stamp) = items[first..]
                .last()
                .and_then(|item| item["timestamp"].as_str())
            {
                last_timestamp = stamp.to_owned();
            }
        }
        items.sort_by_key(|item| item["sequence"].as_u64().unwrap_or(u64::MAX));
        items
    }

    // Window-transition tests compare every entry without the quadratic cost of
    // paging and looking up each of thousands of visible filler records.
    fn assert_fold_matches_upstream(session: &ExternalSession) -> Vec<Value> {
        let expected = unfurled_timeline(session);
        assert_eq!(normalized_timeline(session).unwrap(), expected);
        expected
    }

    fn assert_timeline_slices(session: &ExternalSession) -> Vec<Value> {
        let expected = assert_fold_matches_upstream(session);
        for order in [TimelineOrder::Sequence, TimelineOrder::TimestampSequence] {
            let mut ordered = expected.clone();
            ordered.sort_by(|a, b| timeline_key(b, 0).cmp_in(&timeline_key(a, 0), order));
            let mut walked = Vec::new();
            let mut before = None;
            loop {
                let page = timeline_slice(session, order, before.as_ref(), 7).unwrap();
                let end = ordered.len().min(walked.len() + 7);
                assert_eq!(page.items, ordered[walked.len()..end]);
                walked.extend(page.items);
                assert_eq!(page.has_more, walked.len() < ordered.len());
                if !page.has_more {
                    break;
                }
                before = Some(timeline_key(walked.last().unwrap(), 0));
            }
            assert_eq!(walked, ordered);
            let zero = timeline_slice(session, order, None, 0).unwrap();
            assert!(zero.items.is_empty());
            assert_eq!(zero.has_more, !ordered.is_empty());
        }
        expected
    }

    #[test]
    fn incremental_fold_preserves_native_bytes_and_sources_for_all_line_drivers() {
        use std::io::Write as _;
        let root = tempfile::tempdir().unwrap();
        for driver in [
            ExternalDriver::Claude,
            ExternalDriver::Codex,
            ExternalDriver::Pi,
            ExternalDriver::Omp,
        ] {
            let path = root.path().join(format!("{}.jsonl", driver.as_str()));
            fs::write(
                &path,
                b"\n{\"type\":\"new-kind\",\"value\":{\"kept\":true}}\n",
            )
            .unwrap();
            let session = transcript_session(driver, &path);
            assert_timeline_slices(&session);
            let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
            file.write_all(b"unreadable\xff\n{\"type\":").unwrap();
            assert_timeline_slices(&session);
            file.write_all(b"\"another-new-kind\",\"extra\":true}\n")
                .unwrap();
            let entries = assert_timeline_slices(&session);
            for entry in &entries {
                let locator = serde_json::from_value(entry["_source"].clone()).unwrap();
                let record = normalized_record(&session, &locator).unwrap();
                assert!(
                    record
                        .iter()
                        .any(|item| item["id"] == entry["id"] && item["body"] == entry["body"])
                );
            }
        }
    }

    fn codex_record(id: &str, text: &str) -> String {
        format!(
            "{}\n",
            json!({"type":"response_item","timestamp":"2026-10-07T00:00:00Z",
                "payload":{"type":"message","role":"assistant","id":id,
                    "content":[{"type":"output_text","text":text}]}})
        )
    }

    #[test]
    fn timeline_generation_survives_appends_and_moves_on_in_place_rewrites() {
        use std::io::Write as _;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("generation.jsonl");
        fs::write(
            &path,
            format!(
                "{}{}",
                codex_record("one", "one"),
                codex_record("two", "two")
            ),
        )
        .unwrap();
        let session = transcript_session(ExternalDriver::Codex, &path);
        let order = TimelineOrder::Sequence;
        let first = timeline_slice(&session, order, None, 10).unwrap();
        assert_eq!(texts(&first.items).len(), 2);
        // A pure append keeps the generation, so an outstanding page cursor still applies.
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        write!(file, "{}", codex_record("three", "three")).unwrap();
        let appended = timeline_slice(&session, order, None, 10).unwrap();
        assert_eq!(appended.generation, first.generation);
        // Same inode, truncated and rewritten: every basis input still matches.
        fs::write(&path, codex_record("replaced", "replaced")).unwrap();
        let replaced = timeline_slice(&session, order, None, 10).unwrap();
        assert_ne!(replaced.generation, first.generation);
        assert_eq!(texts(&replaced.items), ["replaced"]);
        // A rewrite of the single retained record moves it again.
        fs::write(&path, codex_record("edited", "edited")).unwrap();
        let edited = timeline_slice(&session, order, None, 10).unwrap();
        assert_ne!(edited.generation, replaced.generation);
        assert_eq!(texts(&edited.items), ["edited"]);
    }

    #[test]
    fn concurrent_append_during_a_parked_refresh_is_read_completely() {
        use std::io::Write as _;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("parked.jsonl");
        fs::write(&path, codex_record("one", "one")).unwrap();
        timeline_test_support::arm_refresh(&path);
        let reader = {
            let session = transcript_session(ExternalDriver::Codex, &path);
            std::thread::spawn(move || normalized_timeline(&session).unwrap())
        };
        timeline_test_support::wait_refresh_arrived(&path);
        // The size was captured and the first record read, but EOF has not been checked.
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        write!(file, "{}", codex_record("two", "two")).unwrap();
        timeline_test_support::release_refresh(&path);
        let parked = reader.join().unwrap();
        assert_eq!(texts(&parked), ["one"]);
        let session = transcript_session(ExternalDriver::Codex, &path);
        let after = timeline_slice(&session, TimelineOrder::Sequence, None, 10).unwrap();
        assert_eq!(texts(&after.items), ["two", "one"]);
    }

    #[test]
    fn opencode_timeline_generation_tracks_database_content() {
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
        let insert = |connection: &Connection, id: &str, created: i64| {
            connection
                .execute(
                    "INSERT INTO message VALUES (?1, 'ses_native', ?2, ?2, ?3)",
                    params![id, created, r#"{"role":"assistant"}"#],
                )
                .unwrap();
        };
        insert(&connection, "msg_1", 1_700_000_000_000);
        let external = ExternalSession {
            id: external_session_id(ExternalDriver::OpenCode, "ses_native"),
            revision: "revision".into(),
            driver: ExternalDriver::OpenCode,
            native_id: "ses_native".into(),
            transcript: database,
            codex_home: None,
            cwd: None,
            title: None,
            started_at_unix_ms: 1_700_000_000_000,
            updated_at_unix_ms: 1_700_000_001_000,
            process: None,
        };
        let order = TimelineOrder::Sequence;
        let first = timeline_slice(&external, order, None, 10).unwrap();
        let again = timeline_slice(&external, order, None, 10).unwrap();
        assert_eq!(first.generation, again.generation);
        insert(&connection, "msg_2", 1_700_000_001_000);
        drop(connection);
        let changed = timeline_slice(&external, order, None, 10).unwrap();
        assert_ne!(changed.generation, first.generation);
    }

    #[test]
    fn incremental_fold_reseeds_cached_suffix_when_session_start_changes() {
        use std::io::Write as _;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("session.jsonl");
        let head = json!({"type":"user","message":{"role":"user","content":"head"}});
        let anchor = claude_line("user", "2026-09-30T10:00:00Z", "anchor");
        fs::write(
            &path,
            format!("{CLAUDE_WINDOW_FILLER}{head}\n{anchor}\n{CLAUDE_WINDOW_FILLER}not JSON"),
        )
        .unwrap();
        let mut session = transcript_session(ExternalDriver::Claude, &path);
        session.started_at_unix_ms = 1000;
        let before = assert_timeline_slices(&session);
        session.started_at_unix_ms = 2000;
        let after = assert_timeline_slices(&session);
        assert_eq!(
            after
                .iter()
                .find(|item| item["body"]["text"] == "head")
                .unwrap()["timestamp"],
            timestamp(2000),
        );
        assert_eq!(
            before
                .iter()
                .find(|item| item["body"]["text"] == "anchor")
                .unwrap()["id"],
            after
                .iter()
                .find(|item| item["body"]["text"] == "anchor")
                .unwrap()["id"],
        );
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        write!(file, "\n{CLAUDE_WINDOW_FILLER}{head}\n").unwrap();
        let appended = assert_timeline_slices(&session);
        assert_eq!(
            appended
                .iter()
                .rev()
                .find(|item| item["body"]["text"] == "head")
                .unwrap()["timestamp"],
            "2026-09-30T10:00:00Z",
        );
    }

    #[test]
    fn a_cold_and_sliding_fold_seed_timestamp_from_the_retained_window() {
        use std::io::Write as _;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("session.jsonl");
        let predecessor = claude_line("user", "2026-09-30T10:00:00Z", "omitted");
        let head = json!({"type":"user","message":{"role":"user","content":[{"type":"text","text":"head"}]}});
        fs::write(
            &path,
            format!(
                "{predecessor}\n{}{head}\n",
                CLAUDE_WINDOW_FILLER.repeat(MAX_TIMELINE_LINES - 2)
            ),
        )
        .unwrap();
        let mut session = transcript_session(ExternalDriver::Claude, &path);
        session.started_at_unix_ms = 1000;
        session.updated_at_unix_ms = 2000;
        let before = assert_fold_matches_upstream(&session);
        assert_eq!(
            before
                .iter()
                .find(|item| item["body"]["text"] == "head")
                .unwrap()["timestamp"],
            "2026-09-30T10:00:00Z"
        );
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(CLAUDE_WINDOW_FILLER.as_bytes()).unwrap();
        let after = assert_fold_matches_upstream(&session);
        assert_eq!(chat_texts(&after), ["head"]);
        assert_eq!(
            after
                .iter()
                .find(|item| item["body"]["text"] == "head")
                .unwrap()["timestamp"],
            timestamp(session.started_at_unix_ms)
        );
    }

    #[test]
    fn incremental_line_fold_matches_upstream_across_the_line_cap() {
        use std::io::Write as _;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("session.jsonl");
        fs::write(&path, CLAUDE_WINDOW_FILLER.repeat(MAX_TIMELINE_LINES - 3)).unwrap();
        let mut session = transcript_session(ExternalDriver::Claude, &path);
        session.started_at_unix_ms = 1000;
        session.updated_at_unix_ms = 2000;
        assert_fold_matches_upstream(&session);
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        for index in 0..8 {
            writeln!(
                file,
                "{}",
                claude_line("user", "2026-09-30T10:00:00Z", &format!("record-{index}"))
            )
            .unwrap();
            assert_fold_matches_upstream(&session);
        }
        file.write_all(b"not JSON\xff\n").unwrap();
        let before = assert_fold_matches_upstream(&session);
        let old = before.iter().find(|item| item["type"] == "error").unwrap();
        writeln!(
            file,
            "{}",
            claude_line("user", "2026-09-30T10:00:01Z", "after error")
        )
        .unwrap();
        let after = assert_fold_matches_upstream(&session);
        let new = after.iter().find(|item| item["type"] == "error").unwrap();
        assert_eq!(new["id"], old["id"]);
        assert_eq!(new["_source"]["offset"], old["_source"]["offset"]);
        assert_eq!(new["body"]["blocks"], old["body"]["blocks"]);
        assert_eq!(
            new["body"]["details"]["line_in_window"].as_u64().unwrap() + 1,
            old["body"]["details"]["line_in_window"].as_u64().unwrap(),
        );
    }

    #[test]
    fn line_fold_rebuilds_after_rewrite_truncation_and_replacement() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("session.jsonl");
        let record = |text| claude_line("user", "2026-09-30T10:00:00Z", text);
        let original = format!("{}\n{}\n", record("first"), record("last"));
        fs::write(&path, &original).unwrap();
        let session = transcript_session(ExternalDriver::Claude, &path);
        assert_eq!(texts(&assert_timeline_slices(&session)), ["first", "last"]);
        let identity = file_identity(&fs::metadata(&path).unwrap());
        let rewritten = format!("{}\n{}\n", record("other"), record("edit"));
        assert_eq!(original.len(), rewritten.len());
        fs::write(&path, rewritten).unwrap();
        assert_eq!(file_identity(&fs::metadata(&path).unwrap()), identity);
        assert_eq!(texts(&assert_timeline_slices(&session)), ["other", "edit"]);
        fs::write(&path, format!("{}\n", record("short"))).unwrap();
        assert_eq!(texts(&assert_timeline_slices(&session)), ["short"]);
        let replacement = root.path().join("replacement.jsonl");
        fs::write(
            &replacement,
            format!("{}\n{}\n", record("replacement"), record("new")),
        )
        .unwrap();
        fs::rename(&replacement, &path).unwrap();
        #[cfg(unix)]
        assert_ne!(file_identity(&fs::metadata(&path).unwrap()), identity);
        assert_eq!(
            texts(&assert_timeline_slices(&session)),
            ["replacement", "new"]
        );
    }

    #[test]
    fn parsed_and_torn_tails_are_refolded_with_pinned_record_sources() {
        use std::io::Write as _;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("session.jsonl");
        let first = claude_line("user", "2026-09-30T10:00:00Z", "question");
        let tail = claude_line("assistant", "2026-09-30T10:00:01Z", "answer");
        fs::write(&path, format!("{first}\n{tail}")).unwrap();
        let session = transcript_session(ExternalDriver::Claude, &path);
        let writing = assert_timeline_slices(&session);
        let changed = tail.replace("answer", "edited");
        fs::write(&path, format!("{first}\n{changed}")).unwrap();
        let edited = assert_timeline_slices(&session);
        assert_eq!(texts(&edited), ["question", "edited"]);
        assert_eq!(
            edited.iter().map(|entry| &entry["id"]).collect::<Vec<_>>(),
            writing.iter().map(|entry| &entry["id"]).collect::<Vec<_>>()
        );
        let locator = serde_json::from_value(edited.last().unwrap()["_source"].clone()).unwrap();
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(file).unwrap();
        let completed = assert_timeline_slices(&session);
        assert_eq!(
            completed.last().unwrap()["id"],
            edited.last().unwrap()["id"]
        );
        assert_ne!(
            completed.last().unwrap()["_source"],
            edited.last().unwrap()["_source"]
        );
        assert_eq!(
            normalized_record(&session, &locator)
                .unwrap()
                .last()
                .unwrap()["body"],
            edited.last().unwrap()["body"]
        );
        let completed_len = file.metadata().unwrap().len();
        file.write_all(b"{\"type\":\"assistant\",\"message\":")
            .unwrap();
        let torn = assert_timeline_slices(&session);
        assert_eq!(
            torn.last().unwrap()["body"]["code"],
            "native-line-unreadable"
        );
        file.write_all(
            b"{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"repaired\"}]}}\n",
        )
        .unwrap();
        assert_eq!(
            texts(&assert_timeline_slices(&session)),
            ["question", "edited", "repaired"]
        );
        file.set_len(completed_len).unwrap();
        assert_eq!(assert_timeline_slices(&session), completed);
    }

    #[test]
    fn removing_a_tail_restores_the_full_completed_line_window() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("session.jsonl");
        let first = claude_line("user", "2026-09-30T10:00:00Z", "first");
        let tail = claude_line("assistant", "2026-09-30T10:00:01Z", "tail");
        let completed = format!(
            "{first}\n{}",
            CLAUDE_WINDOW_FILLER.repeat(MAX_TIMELINE_LINES - 1)
        );
        fs::write(&path, format!("{completed}{tail}")).unwrap();
        let session = transcript_session(ExternalDriver::Claude, &path);
        assert_eq!(
            chat_texts(&assert_fold_matches_upstream(&session)),
            ["tail"]
        );
        fs::write(&path, completed).unwrap();
        assert_eq!(
            chat_texts(&assert_fold_matches_upstream(&session)),
            ["first"]
        );
    }

    #[test]
    fn timeline_slices_preserve_each_order_with_truncation_and_mixed_timestamps() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("session.jsonl");
        let stamps = [
            "2026-09-30T10:30:00+02:00",
            "2026-09-30T10:00:00.000Z",
            "2026-09-30T10:00:00Z",
            "2026-09-30T09:00:00Z",
        ];
        let records = (0..23)
            .map(|index| {
                format!(
                    "{}\n",
                    claude_line(
                        "user",
                        stamps[index % stamps.len()],
                        &format!("record-{index}")
                    ),
                )
            })
            .collect::<String>();
        fs::write(
            &path,
            format!(
                "{}{records}",
                CLAUDE_WINDOW_FILLER.repeat(MAX_TIMELINE_LINES)
            ),
        )
        .unwrap();
        let session = transcript_session(ExternalDriver::Claude, &path);
        assert_timeline_slices(&session);
        let sequence = timeline_slice(&session, TimelineOrder::Sequence, None, usize::MAX).unwrap();
        assert_eq!(sequence.items[0]["body"]["text"], "record-22");
        assert_eq!(sequence.items.last().unwrap()["type"], "truncation");
        let timed =
            timeline_slice(&session, TimelineOrder::TimestampSequence, None, usize::MAX).unwrap();
        assert_ne!(sequence.items, timed.items);
        assert_eq!(timed.items[0]["timestamp"], stamps[0]);
    }

    #[test]
    fn timeline_slices_and_entry_lookup_follow_byte_window_slides() {
        use std::io::Write as _;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("session.jsonl");
        let records = |label: &str, count| {
            (0..count)
                .map(|index| {
                    format!(
                        "{}\n",
                        claude_line("user", "2026-09-30T10:00:00Z", &format!("{label}-{index}")),
                    )
                })
                .collect::<String>()
        };
        let prefix = records("old", 18);
        let suffix = records("recent", 16);
        let mut file = File::create(&path).unwrap();
        file.write_all(prefix.as_bytes()).unwrap();
        // A single omitted JSON record spans the window without expanded raw-text payloads.
        let opening = b"{\"type\":\"file-history-snapshot\",\"padding\":\"";
        let closing = b"\"}\n";
        let padding = MAX_TIMELINE_BYTES as usize
            - prefix.len()
            - suffix.len()
            - opening.len()
            - closing.len();
        file.write_all(opening).unwrap();
        file.write_all(&vec![b' '; padding]).unwrap();
        file.write_all(closing).unwrap();
        file.write_all(suffix.as_bytes()).unwrap();
        drop(file);
        let session = transcript_session(ExternalDriver::Claude, &path);
        let initial = assert_timeline_slices(&session);
        let old = initial
            .iter()
            .find(|item| item["body"]["text"] == "old-0")
            .unwrap();
        let survivor = initial
            .iter()
            .find(|item| item["body"]["text"] == "recent-0")
            .unwrap();
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(records("appended", 16).as_bytes()).unwrap();
        let slid = assert_timeline_slices(&session);
        assert!(slid.iter().any(|item| item["type"] == "truncation"));
        assert!(!slid.iter().any(|item| item["id"] == old["id"]));
        let retained = slid
            .iter()
            .find(|item| item["id"] == survivor["id"])
            .unwrap();
        assert_eq!(retained["body"], survivor["body"]);
        assert_eq!(retained["_source"]["offset"], survivor["_source"]["offset"]);
        assert_ne!(
            retained["_source"]["line_index"],
            survivor["_source"]["line_index"]
        );
    }

    #[test]
    fn fold_cache_evicts_to_source_byte_and_transcript_budgets() {
        let budget = 2 * (claude_line("user", "2026-09-30T10:00:00Z", "small").len() + 1);
        let root = tempfile::tempdir().unwrap();
        let mut cache = LineFoldCache::new();
        let mut expected_bytes = 0;
        for index in 0..4 {
            let path = root.path().join(format!("{index}.jsonl"));
            let text = if index == 3 {
                "x".repeat(budget)
            } else {
                "small".into()
            };
            let record = format!("{}\n", claude_line("user", "2026-09-30T10:00:00Z", &text));
            fs::write(&path, &record).unwrap();
            let session = transcript_session(ExternalDriver::Claude, &path);
            let fold = SharedLineFold::default();
            let bytes = {
                let mut state = match fold.lock() {
                    Ok(state) => state,
                    Err(error) => panic!("test fold poisoned: {error}"),
                };
                assert_eq!(texts(&state.read(&session).unwrap()), [text.as_str()]);
                assert_eq!(state.retained_source_bytes, record.len());
                state.retained_source_bytes
            };
            cache.push_back((path.clone(), session.driver, fold, bytes));
            trim_line_folds(&mut cache, budget);
            assert!(cache.iter().map(|entry| entry.3).sum::<usize>() <= budget);
            if bytes > budget {
                assert!(!cache.iter().any(|entry| entry.0 == path));
                assert_eq!(
                    cache.iter().map(|entry| entry.3).sum::<usize>(),
                    expected_bytes
                );
            } else {
                assert_eq!(cache.back().unwrap().0, path);
                if index == 2 {
                    assert_eq!(cache.front().unwrap().0, root.path().join("1.jsonl"));
                }
                expected_bytes = cache.iter().map(|entry| entry.3).sum();
            }
        }
        cache.clear();
        for index in 0..MAX_FOLDED_TRANSCRIPTS + 2 {
            cache.push_back((
                root.path().join(format!("{index}.jsonl")),
                ExternalDriver::Claude,
                SharedLineFold::default(),
                0,
            ));
            trim_line_folds(&mut cache, budget);
        }
        assert_eq!(cache.len(), MAX_FOLDED_TRANSCRIPTS);
        assert_eq!(cache.front().unwrap().0, root.path().join("2.jsonl"));
    }

    #[test]
    #[ignore = "benchmark over a local corpus"]
    fn conversation_ivm_bench() {
        use std::io::Write as _;
        let Ok(corpus) = std::env::var("ST_CONV_IVM_CORPUS") else {
            return;
        };
        let median = |mut samples: Vec<f64>| {
            samples.sort_by(f64::total_cmp);
            samples[samples.len() / 2]
        };
        let ms = |start: Instant| start.elapsed().as_secs_f64() * 1000.0;
        let text = fs::read(corpus).unwrap();
        let base = text.len().saturating_sub(40 << 20);
        let base = if base == 0 {
            0
        } else {
            base + text[base..]
                .iter()
                .position(|byte| *byte == b'\n')
                .expect("corpus must contain complete records")
                + 1
        };
        let text = &text[base..];
        let mut cuts = text
            .iter()
            .enumerate()
            .filter(|(_, byte)| **byte == b'\n')
            .map(|(index, _)| index + 1)
            .collect::<Vec<_>>();
        assert!(
            cuts.len() >= 2,
            "corpus must contain at least two complete records"
        );
        cuts = cuts.split_off(cuts.len().saturating_sub(17));
        let root = tempfile::tempdir().unwrap();
        let mut cold = Vec::new();
        let mut incremental = Vec::new();
        let mut first_page = Vec::new();
        let mut items = 0;
        for (run, window) in cuts.windows(2).enumerate() {
            let path = root.path().join(format!("run-{run}.jsonl"));
            fs::write(&path, &text[..window[0]]).unwrap();
            let session = transcript_session(ExternalDriver::Omp, &path);
            let start = Instant::now();
            normalized_timeline(&session).unwrap();
            cold.push(ms(start));
            let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
            file.write_all(&text[window[0]..window[1]]).unwrap();
            let start = Instant::now();
            let after = normalized_timeline(&session).unwrap();
            incremental.push(ms(start));
            items = after.len();
            fs::write(&path, &text[..window[0]]).unwrap();
            normalized_timeline(&session).unwrap();
            let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
            file.write_all(&text[window[0]..window[1]]).unwrap();
            let start = Instant::now();
            let page =
                timeline_slice(&session, TimelineOrder::TimestampSequence, None, 200).unwrap();
            first_page.push(ms(start));
            let mut expected = after;
            expected.sort_by(|a, b| {
                timeline_key(b, 0).cmp_in(&timeline_key(a, 0), TimelineOrder::TimestampSequence)
            });
            expected.truncate(200);
            assert_eq!(page.items, expected);
            assert_eq!(page.has_more, items > 200);
        }
        eprintln!(
            "corpus_bytes={} window_items={items} cold_p50_ms={:.3} cold_max_ms={:.3} incremental_p50_ms={:.3} incremental_max_ms={:.3} first_page200_after_append_p50_ms={:.3} first_page200_after_append_max_ms={:.3}",
            text.len(),
            median(cold.clone()),
            cold.iter().copied().fold(0.0, f64::max),
            median(incremental.clone()),
            incremental.iter().copied().fold(0.0, f64::max),
            median(first_page.clone()),
            first_page.iter().copied().fold(0.0, f64::max),
        );
    }
}
