use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::fs::{self, File};
use std::io::{BufRead as _, BufReader, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};
#[cfg(not(target_os = "linux"))]
use std::process::Command;
use std::sync::{Mutex, OnceLock};
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
const MAX_EXPOSED_HISTORY: usize = 2_000;
const MAX_METADATA_LINES: usize = 64;
const MAX_TIMELINE_LINES: usize = 4_096;
const MAX_TIMELINE_BYTES: u64 = 32 * 1024 * 1024;
// A maximum-size page must remain below the client gateway's one-megabyte response ceiling even
// when every native entry contains a large tool payload.
const MAX_TIMELINE_VALUE_BYTES: usize = 8 * 1024;
const DISCOVERY_CACHE_TTL: Duration = Duration::from_secs(2);

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

#[derive(Clone)]
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
    static CACHE: OnceLock<Mutex<Option<(PathBuf, Instant, ExternalDiscovery)>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(None));
    let mut cache = cache.lock().expect("external session cache mutex poisoned");
    if let Some((cached_home, created, discovery)) = cache.as_ref()
        && cached_home == home
        && created.elapsed() <= DISCOVERY_CACHE_TTL
    {
        return Ok(filter_discovery(discovery.clone(), include_history));
    }
    let discovery = discover_uncached(home)?;
    *cache = Some((home.to_owned(), Instant::now(), discovery.clone()));
    Ok(filter_discovery(discovery, include_history))
}

pub(crate) fn discover_fresh(
    home: Option<&Path>,
    include_history: bool,
) -> Result<ExternalDiscovery> {
    let Some(home) = home else {
        return Ok(ExternalDiscovery::default());
    };
    Ok(filter_discovery(discover_uncached(home)?, include_history))
}

fn discover_uncached(home: &Path) -> Result<ExternalDiscovery> {
    let mut metadata = discover_files(home)?;
    metadata.sort_by(|left, right| {
        right
            .updated_at_unix_ms
            .cmp(&left.updated_at_unix_ms)
            .then_with(|| left.native_id.cmp(&right.native_id))
    });
    metadata.truncate(MAX_EXPOSED_HISTORY);

    let candidates = platform_processes()?;
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
    Ok(ExternalDiscovery {
        sessions,
        unresolved_processes,
    })
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
    if !id.starts_with("session/external-") {
        return Ok(None);
    }
    Ok(discover(home, true)?
        .sessions
        .into_iter()
        .find(|item| item.id == id))
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
    for entry in WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
    {
        if !entry.file_type().is_file() || entry.file_name() != std::ffi::OsStr::new(&filename) {
            continue;
        }
        let Some(metadata) = read_metadata(driver, entry.path())? else {
            continue;
        };
        if metadata.native_id != native_id {
            continue;
        }
        return Ok(Some(ExternalSession {
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
        }));
    }
    Ok(None)
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
    let mut files = Vec::new();
    for (index, entry) in entries.enumerate() {
        anyhow::ensure!(
            index < MAX_DISCOVERED_FILES,
            "managed OMP session directory exceeds the bounded file inventory"
        );
        let entry = entry?;
        if entry.file_type()?.is_file()
            && entry.path().extension().is_some_and(|ext| ext == "jsonl")
        {
            files.push(entry.path());
        }
    }
    // OMP names sessions with an ISO timestamp prefix. Try a bounded number
    // of newest candidates so a partial header cannot hide a valid predecessor.
    files.sort_unstable_by(|left, right| right.file_name().cmp(&left.file_name()));
    for path in files.into_iter().take(64) {
        let Some(metadata) = read_metadata(ExternalDriver::Omp, &path)? else {
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
    Ok(discover_fresh(home, true)?
        .sessions
        .into_iter()
        .find(|item| item.id == id))
}

pub(crate) fn timestamp(unix_ms: u128) -> String {
    let millis = i64::try_from(unix_ms).unwrap_or(i64::MAX);
    DateTime::<Utc>::from_timestamp_millis(millis)
        .unwrap_or(DateTime::<Utc>::UNIX_EPOCH)
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

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
    if start != 0 {
        let mut partial = String::new();
        reader.read_line(&mut partial)?;
    }
    let mut lines = VecDeque::new();
    for line in reader.lines() {
        if lines.len() == MAX_TIMELINE_LINES {
            lines.pop_front();
        }
        lines.push_back(line?);
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
    for (line_index, line) in lines.into_iter().enumerate() {
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let sequence = ((line_index as u64).saturating_add(1)).saturating_mul(16);
        match session.driver {
            ExternalDriver::Codex => normalize_codex(&value, sequence, session, &mut items),
            ExternalDriver::Claude => normalize_claude(&value, sequence, session, &mut items),
            ExternalDriver::Pi | ExternalDriver::Omp => {
                normalize_omp(&value, sequence, session, &mut items)
            }
            ExternalDriver::OpenCode => unreachable!("OpenCode history is stored in SQLite"),
        }
    }
    items.sort_by_key(|item| item["sequence"].as_u64().unwrap_or(u64::MAX));
    Ok(items)
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
            args.entries_mut()
                .push(KdlEntry::new(format!("--resume={}", session.native_id)));
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

fn discover_files(home: &Path) -> Result<Vec<SessionMetadata>> {
    let roots = [
        (ExternalDriver::Codex, home.join(".codex/sessions")),
        (ExternalDriver::Claude, home.join(".claude/projects")),
        (ExternalDriver::Pi, home.join(".pi/agent/sessions")),
        (ExternalDriver::Omp, home.join(".oh-omp/agent/sessions")),
        (ExternalDriver::Omp, home.join(".omp/agent/sessions")),
    ];
    let mut found = discover_opencode_sessions(home)?;
    for (driver, root) in roots {
        if !root.is_dir() {
            continue;
        }
        for entry in WalkDir::new(root)
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
            if let Some(metadata) = read_metadata(driver, entry.path())? {
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
    for line in BufReader::new(file).lines().take(MAX_METADATA_LINES) {
        let Ok(value) = serde_json::from_str::<Value>(&line?) else {
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
    if driver == ExternalDriver::Claude && native_id.is_none() {
        native_id = path
            .file_stem()
            .and_then(|value| value.to_str())
            .map(str::to_owned);
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
    let output = Command::new("ps")
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
    let output = Command::new("lsof")
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
    let messages = message_statement
        .query_map(
            params![session.native_id, MAX_TIMELINE_LINES as i64 + 1],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut messages = messages;
    let mut truncated = messages.len() > MAX_TIMELINE_LINES;
    if truncated {
        messages.remove(0);
    }
    let mut part_statement = connection.prepare(
        "SELECT data FROM part WHERE session_id = ?1 AND message_id = ?2 \
         ORDER BY time_created, id",
    )?;
    let mut items = VecDeque::new();
    // Include the surrounding JSON array delimiters so this remains an exact bound on the
    // serialized timeline, not just on its native payloads.
    let mut serialized_bytes = 2_usize;
    let mut sequence = 1_u64;
    for (message_id, created, encoded) in messages {
        let message = serde_json::from_str::<Value>(&encoded).unwrap_or(Value::Null);
        let role = normalized_role(message.get("role").and_then(Value::as_str));
        let at = timestamp(created.max(0) as u128);
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
        let parts = part_statement.query_map(params![session.native_id, message_id], |row| {
            row.get::<_, String>(0)
        })?;
        for encoded_part in parts {
            let encoded_part = encoded_part?;
            let Ok(part) = serde_json::from_str::<Value>(&encoded_part) else {
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
                    if matches!(
                        state.get("status").and_then(Value::as_str),
                        Some("completed" | "error")
                    ) {
                        push_tool_result(
                            &mut additions,
                            next_opencode_sequence(&mut sequence)?,
                            &at,
                            call_id,
                            state
                                .get("output")
                                .or_else(|| state.get("error"))
                                .cloned()
                                .unwrap_or(Value::Null),
                        );
                    }
                }
                _ => {}
            }
            extend_bounded_opencode_timeline(
                &mut items,
                &mut serialized_bytes,
                &mut truncated,
                additions,
            );
        }
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

fn normalize_codex(
    value: &Value,
    sequence: u64,
    session: &ExternalSession,
    items: &mut Vec<Value>,
) {
    let timestamp = value
        .get("timestamp")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| timestamp(session.updated_at_unix_ms));
    let payload = &value["payload"];
    if value["type"] != "response_item" {
        return;
    }
    match payload["type"].as_str() {
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
            if let Some(content) = payload["content"].as_array() {
                for (offset, part) in content.iter().enumerate() {
                    if let Some(text) = part
                        .get("text")
                        .or_else(|| part.get("input_text"))
                        .or_else(|| part.get("output_text"))
                        .and_then(Value::as_str)
                    {
                        push_content(items, sequence + 1 + offset as u64, &timestamp, role, text);
                    }
                }
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
        _ => {}
    }
}

fn normalize_claude(
    value: &Value,
    sequence: u64,
    session: &ExternalSession,
    items: &mut Vec<Value>,
) {
    let Some(kind @ ("user" | "assistant")) = value["type"].as_str() else {
        return;
    };
    let role = if kind == "user" { "user" } else { "assistant" };
    let timestamp = value["timestamp"]
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| timestamp(session.updated_at_unix_ms));
    let message_id = value
        .pointer("/message/id")
        .and_then(Value::as_str)
        .or_else(|| value.get("uuid").and_then(Value::as_str))
        .map(str::to_owned)
        .unwrap_or_else(|| format!("native/{sequence}"));
    push_message(items, sequence, &timestamp, role, &message_id);
    match &value["message"]["content"] {
        Value::String(text) => push_content(items, sequence + 1, &timestamp, role, text),
        Value::Array(parts) => {
            for (offset, part) in parts.iter().enumerate() {
                let item_sequence = sequence + 1 + offset as u64;
                match part["type"].as_str() {
                    Some("text") => {
                        if let Some(text) = part["text"].as_str() {
                            push_content(items, item_sequence, &timestamp, role, text);
                        }
                    }
                    Some("tool_use") => push_tool_call(
                        items,
                        item_sequence,
                        &timestamp,
                        part["id"].as_str().unwrap_or("native-call"),
                        part["name"].as_str().unwrap_or("tool"),
                        part.get("input").cloned().unwrap_or_else(|| json!({})),
                    ),
                    Some("tool_result") => push_tool_result(
                        items,
                        item_sequence,
                        &timestamp,
                        part["tool_use_id"].as_str().unwrap_or("native-call"),
                        part.get("content").cloned().unwrap_or(Value::Null),
                    ),
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

fn normalize_omp(value: &Value, sequence: u64, session: &ExternalSession, items: &mut Vec<Value>) {
    if value["type"] != "message" {
        return;
    }
    let message = &value["message"];
    let role = normalized_role(message["role"].as_str());
    let timestamp = value["timestamp"]
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| timestamp(session.updated_at_unix_ms));
    let message_id = value["id"]
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| format!("native/{sequence}"));
    push_message(items, sequence, &timestamp, role, &message_id);
    match &message["content"] {
        Value::String(text) => push_content(items, sequence + 1, &timestamp, role, text),
        Value::Array(parts) => {
            for (offset, part) in parts.iter().enumerate() {
                let item_sequence = sequence + 1 + offset as u64;
                match part["type"].as_str() {
                    Some("text") => {
                        if let Some(text) = part["text"].as_str() {
                            push_content(items, item_sequence, &timestamp, role, text);
                        }
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
                        part.get("content").cloned().unwrap_or(Value::Null),
                    ),
                    _ => {}
                }
            }
        }
        _ => {}
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
    let content = bounded_value(content);
    items.push(timeline_item(sequence, timestamp, "tool", "tool_result", json!({"call_id":call_id, "status":"success", "media_type":"application/json", "content":content})));
}

fn bounded_text(value: &str) -> String {
    if value.len() <= MAX_TIMELINE_VALUE_BYTES {
        return value.to_owned();
    }
    let mut output = value
        .chars()
        .take(MAX_TIMELINE_VALUE_BYTES)
        .collect::<String>();
    output.push_str("\n[st3 truncated this native timeline value]");
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
        let session = |driver| ExternalSession {
            id: "session/external-test".into(),
            revision: "revision".into(),
            driver,
            native_id: "native".into(),
            transcript: PathBuf::new(),
            cwd: None,
            title: None,
            started_at_unix_ms: 0,
            updated_at_unix_ms: 0,
            process: None,
        };
        let mut items = Vec::new();
        normalize_codex(
            &json!({"type":"response_item","timestamp":"2026-01-01T00:00:00Z","payload":{"type":"message","role":"assistant","id":"m1","content":[{"type":"output_text","text":"codex"}]}}),
            0,
            &session(ExternalDriver::Codex),
            &mut items,
        );
        normalize_claude(
            &json!({"type":"assistant","timestamp":"2026-01-01T00:00:01Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"c1","name":"shell","input":{"command":"true"}}]}}),
            16,
            &session(ExternalDriver::Claude),
            &mut items,
        );
        normalize_omp(
            &json!({"type":"message","id":"m2","timestamp":"2026-01-01T00:00:02Z","message":{"role":"tool","content":[{"type":"toolResult","toolCallId":"c1","content":"ok"}]}}),
            32,
            &session(ExternalDriver::Omp),
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

    #[test]
    fn current_codex_custom_tools_are_visible_but_bootstrap_prompts_are_not() {
        let session = ExternalSession {
            id: "session/external-current-codex".into(),
            revision: "revision".into(),
            driver: ExternalDriver::Codex,
            native_id: "native".into(),
            transcript: PathBuf::new(),
            cwd: None,
            title: None,
            started_at_unix_ms: 0,
            updated_at_unix_ms: 0,
            process: None,
        };
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
                &session,
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
    fn imported_sessions_render_as_valid_durable_seats() {
        let workspace = tempfile::tempdir().unwrap();
        for (driver, expected) in [
            (ExternalDriver::Codex, vec!["resume", "native-session"]),
            (ExternalDriver::Claude, vec!["--resume", "native-session"]),
            (ExternalDriver::Pi, vec!["--session"]),
            (ExternalDriver::Omp, vec!["--resume=native-session"]),
            (
                ExternalDriver::OpenCode,
                vec!["--session", "native-session"],
            ),
        ] {
            let session = ExternalSession {
                id: "session/external-test".into(),
                revision: "revision".into(),
                driver,
                native_id: "native-session".into(),
                transcript: workspace.path().join("session.jsonl"),
                cwd: Some(workspace.path().to_owned()),
                title: None,
                started_at_unix_ms: 0,
                updated_at_unix_ms: 0,
                process: None,
            };
            let import = import_seat(&session).unwrap();
            crate::graph::parse_intent(&import.kdl, "host/test").unwrap();
            assert!(import.subject.starts_with("agent/import/"));
            assert!(!import.kdl.contains("mission \""));
            for argument in expected {
                assert!(
                    import.kdl.contains(argument),
                    "{} lacks {argument}",
                    import.kdl
                );
            }
        }
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
            driver_for_command("/Users/test/.opencode/bin/opencode --session ses_123"),
            Some(ExternalDriver::OpenCode)
        );
        assert_eq!(driver_for_command("rg codex crates/st3"), None);
        assert_eq!(driver_for_command("bash -c echo claude"), None);
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
