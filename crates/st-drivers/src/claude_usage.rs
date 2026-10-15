//! Observe completed Claude responses when the native transcript is written.
//!
//! Stop runs before Claude flushes its last reply. A session-owned, shallow filesystem watch
//! therefore owns usage acquisition; it does not wait for another turn or poll the transcript.
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{BufRead as _, BufReader, Seek as _, SeekFrom};
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use anyhow::{Context as _, Result};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher as _};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::driver_paths::Paths;
use crate::flock::{FileLock, Mode, Open, open};
use crate::harness_timeline::{EntryType, Role, Writer};

#[derive(Default, Deserialize, Serialize)]
struct Cursor {
    session_id: String,
    offset: u64,
    #[serde(default)]
    turn_id: String,
    #[serde(default)]
    pending_turns: BTreeMap<String, String>,
}

/// The Stop cursor is an explicit migration boundary: retain earlier usage as recorded rather
/// than replaying historical responses into today's spending. Scan its prefix for turn lineage
/// once, without publishing it. A new session begins at byte zero.
fn initial_cursor(agent_dir: &Path, session: &str, child: Option<&str>) -> Result<Cursor> {
    if let Some(name) = child {
        #[derive(Deserialize)]
        struct Children {
            session_id: String,
            offsets: BTreeMap<String, u64>,
        }
        let path = agent_dir.join(".harness-timeline-claude-subagent-cursors");
        let prior = match fs::read(path) {
            Ok(bytes) => Some(serde_json::from_slice::<Children>(&bytes)?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        return Ok(Cursor {
            session_id: session.to_owned(),
            offset: prior
                .filter(|x| x.session_id == session)
                .and_then(|x| x.offsets.get(name).copied())
                .unwrap_or(0),
            ..Cursor::default()
        });
    }
    let path = agent_dir.join(".harness-timeline-claude-cursor");
    let prior = match fs::read(path) {
        Ok(bytes) => serde_json::from_slice::<Cursor>(&bytes)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Cursor::default(),
        Err(error) => return Err(error.into()),
    };
    Ok(if prior.session_id == session {
        prior
    } else {
        Cursor::default()
    })
}

fn drain(writer: &mut Writer, agent_dir: &Path, transcript: &Path, projects: &Path) -> Result<()> {
    let transcript = transcript.canonicalize()?;
    anyhow::ensure!(
        transcript.starts_with(projects),
        "Claude usage transcript is outside its configured projects"
    );
    let session = transcript
        .file_stem()
        .and_then(|x| x.to_str())
        .context("Claude usage transcript has no session")?;
    anyhow::ensure!(
        transcript.extension().is_some_and(|x| x == "jsonl"),
        "Claude usage transcript is not JSONL"
    );
    drain_file(writer, agent_dir, &transcript, projects, session, None)?;
    let children = transcript.parent().unwrap().join(session).join("subagents");
    let children = match children.canonicalize() {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    anyhow::ensure!(
        children.starts_with(projects),
        "Claude subagent directory is outside its configured projects"
    );
    let entries = match fs::read_dir(&children) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with("agent-") && name.ends_with(".jsonl") {
            anyhow::ensure!(
                entry.file_type()?.is_file(),
                "Claude subagent transcript is not a regular file"
            );
            paths.push((name, entry.path()));
        }
    }
    anyhow::ensure!(
        paths.len() <= 1024,
        "Claude subagent transcript inventory exceeds its bound"
    );
    paths.sort();
    for (name, path) in paths {
        drain_file(writer, agent_dir, &path, projects, session, Some(&name))?;
    }
    Ok(())
}

fn drain_file(
    writer: &mut Writer,
    agent_dir: &Path,
    transcript: &Path,
    projects: &Path,
    session: &str,
    child: Option<&str>,
) -> Result<()> {
    let transcript = transcript.canonicalize()?;
    anyhow::ensure!(
        transcript.starts_with(projects),
        "Claude usage transcript is outside its configured projects"
    );
    let _held = FileLock::hold_blocking(
        open(&agent_dir.join(".claude-usage-cursor.lock"), Open::Create)?,
        Mode::Exclusive,
    )?;
    let cursor_path = agent_dir.join(child.map_or_else(
        || ".claude-usage-cursor.json".to_owned(),
        |name| format!(".claude-usage-{name}.cursor.json"),
    ));
    let existing = match fs::read(&cursor_path) {
        Ok(bytes) => Some(serde_json::from_slice::<Cursor>(&bytes)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let retained = existing.as_ref().is_some_and(|x| x.session_id == session);
    let mut cursor = match existing.filter(|x| x.session_id == session) {
        Some(cursor) => cursor,
        None => initial_cursor(agent_dir, session, child)?,
    };
    let migrate_through = if cursor.session_id.is_empty() {
        0
    } else if cursor.turn_id.is_empty() {
        cursor.offset
    } else {
        0
    };
    let file = fs::File::open(&transcript)?;
    anyhow::ensure!(
        cursor.offset <= file.metadata()?.len(),
        "Claude usage transcript shrank behind its cursor"
    );
    if retained && cursor.offset == file.metadata()?.len() {
        return Ok(());
    }
    let mut reader = BufReader::new(file);
    if migrate_through != 0 {
        cursor.offset = 0;
    }
    reader.seek(SeekFrom::Start(cursor.offset))?;
    cursor.session_id = session.to_owned();
    loop {
        let mut line = String::new();
        let length = reader.read_line(&mut line)?;
        if length == 0 || !line.ends_with('\n') {
            break;
        }
        let event: Value =
            serde_json::from_str(&line).context("parsing complete Claude usage transcript line")?;
        cursor.offset += length as u64;
        if child.is_none()
            && event
                .get("sessionId")
                .and_then(Value::as_str)
                .is_some_and(|x| x != session)
        {
            continue;
        }
        let tool_result = event
            .pointer("/message/content")
            .and_then(Value::as_array)
            .is_some_and(|parts| {
                !parts.is_empty() && parts.iter().all(|x| x["type"] == "tool_result")
            });
        if child.is_none()
            && event["type"] == "user"
            && !tool_result
            && event["isCompactSummary"] != true
            && let Some(id) = event["uuid"].as_str()
        {
            cursor.turn_id = id.to_owned();
        }
        if event["type"] != "assistant"
            || !event
                .pointer("/message/usage")
                .is_some_and(Value::is_object)
        {
            continue;
        }
        if event
            .pointer("/message/model")
            .and_then(Value::as_str)
            .is_some_and(|x| x.contains("synthetic"))
        {
            continue;
        }
        let message = event
            .pointer("/message/id")
            .and_then(Value::as_str)
            .context("Claude usage response has no id")?;
        if let Some(name) = child {
            cursor.turn_id = format!(
                "subagent:{}",
                name.trim_start_matches("agent-").trim_end_matches(".jsonl")
            );
        }
        let turn = cursor
            .pending_turns
            .entry(message.to_owned())
            .or_insert_with(|| cursor.turn_id.clone())
            .clone();
        // Native streaming records can precede the completed response. Never freeze provisional
        // counters in the response ledger; preserve the first turn until a completed record.
        if !event
            .pointer("/message/stop_reason")
            .is_some_and(Value::is_string)
        {
            continue;
        }
        cursor.pending_turns.remove(message);
        if cursor.offset <= migrate_through {
            continue;
        }
        anyhow::ensure!(
            !turn.is_empty(),
            "Claude usage response has no native user-turn lineage"
        );
        if let Some(body) = crate::harness_timeline::claude_usage_body(&event, &turn) {
            writer.append(
                format!("claude:{session}:{message}:usage"),
                Role::System,
                EntryType::Usage,
                body,
                true,
            )?;
        }
    }
    anyhow::ensure!(
        cursor.pending_turns.len() <= 1024,
        "Too many unfinished Claude usage responses"
    );
    crate::fsatomic::replace(
        &cursor_path,
        &serde_json::to_vec(&cursor)?,
        crate::fsatomic::Staging::new(".claude-usage-cursor"),
        crate::fsatomic::Durability::FsyncFileAndDir,
    )
    .context("publishing the Claude usage cursor")
}

enum Wake {
    Changed,
    Stop,
}

fn future_path(path: &Path) -> Result<PathBuf> {
    match path.canonicalize() {
        Ok(path) => Ok(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = path
                .parent()
                .context("Claude watch path has no existing ancestor")?;
            let name = path.file_name().context("Claude watch path has no name")?;
            Ok(future_path(parent)?.join(name))
        }
        Err(error) => Err(error.into()),
    }
}

fn refresh_watches(
    watch: &mut RecommendedWatcher,
    watched: &mut BTreeMap<PathBuf, (u64, u64)>,
    paths: &Paths,
    projects: &Path,
    transcript: Option<&Path>,
) -> Result<()> {
    let mut desired = BTreeMap::new();
    for path in [&paths.agent_dir, &paths.session_dir, projects] {
        let metadata = fs::metadata(path)?;
        desired.insert(path.to_path_buf(), (metadata.dev(), metadata.ino()));
    }
    if let Some(transcript) = transcript {
        let parent = transcript
            .parent()
            .context("Claude transcript has no parent")?;
        let native = parent.join(
            transcript
                .file_stem()
                .context("Claude transcript has no session")?,
        );
        for path in [
            parent.to_path_buf(),
            native.clone(),
            native.join("subagents"),
        ] {
            let canonical = match path.canonicalize() {
                Ok(path) => path,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            anyhow::ensure!(
                canonical.starts_with(projects),
                "Claude usage watch directory is outside its configured projects"
            );
            let metadata = fs::metadata(&canonical)?;
            anyhow::ensure!(
                metadata.is_dir(),
                "Claude usage watch target is not a directory"
            );
            desired.insert(canonical, (metadata.dev(), metadata.ino()));
        }
    }
    // Retire old native-session subscriptions, and replace watches on a recreated inode.
    // Nonexistent directories are observed through their nearest existing parent first.
    for path in watched.keys().cloned().collect::<Vec<_>>() {
        if watched.get(&path) != desired.get(&path) {
            if let Err(error) = watch.unwatch(&path)
                && !matches!(
                    error.kind,
                    notify::ErrorKind::WatchNotFound | notify::ErrorKind::PathNotFound
                )
            {
                return Err(error.into());
            }
            watched.remove(&path);
        }
    }
    for (path, identity) in desired {
        if let std::collections::btree_map::Entry::Vacant(entry) = watched.entry(path) {
            watch.watch(entry.key(), RecursiveMode::NonRecursive)?;
            entry.insert(identity);
        }
    }
    Ok(())
}

#[derive(Default)]
struct EventFilter {
    bindings: Vec<PathBuf>,
    transcript: Option<PathBuf>,
}

impl EventFilter {
    fn relevant(&self, path: &Path) -> bool {
        if self.bindings.iter().any(|binding| binding == path) {
            return true;
        }
        let Some(transcript) = &self.transcript else {
            return false;
        };
        let parent = transcript.parent().unwrap();
        let native = parent.join(transcript.file_stem().unwrap());
        let children = native.join("subagents");
        path == transcript
            || path == parent
            || path == native
            || path == children
            || (path.parent() == Some(children.as_path())
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("agent-") && name.ends_with(".jsonl")))
    }
}

#[derive(Default)]
struct Changes {
    changed: bool,
    errors: BTreeSet<String>,
}

#[derive(Clone)]
struct Events {
    send: SyncSender<Wake>,
    filter: Arc<Mutex<EventFilter>>,
    pending: Arc<Mutex<Changes>>,
}

impl Events {
    fn record(&self, event: notify::Result<notify::Event>) {
        let mut pending = self.pending.lock().unwrap();
        match event {
            Ok(event)
                if !matches!(event.kind, EventKind::Access(_))
                    && event
                        .paths
                        .iter()
                        .any(|path| self.filter.lock().unwrap().relevant(path)) =>
            {
                pending.changed = true;
            }
            Err(error) => {
                pending.errors.insert(error.to_string());
            }
            _ => return,
        }
        // One queued token represents the whole pending batch. More streaming writes update
        // the batch instead of growing an unbounded queue or blocking the notify callback.
        let _ = self.send.try_send(Wake::Changed);
    }
}

#[derive(Deserialize, Serialize)]
struct ObservationStatus {
    incarnation: String,
    available: bool,
    errors: BTreeMap<String, u64>,
}

struct Reporter {
    path: PathBuf,
    status: ObservationStatus,
    initialized: bool,
    persistence_error: Option<String>,
}

impl Reporter {
    fn new(agent: &Path, incarnation: &str) -> Self {
        Self {
            path: agent.join("claude-usage-observation.json"),
            status: ObservationStatus {
                incarnation: incarnation.into(),
                available: false,
                errors: BTreeMap::new(),
            },
            initialized: false,
            persistence_error: None,
        }
    }

    fn persist(&mut self) {
        if let Err(error) = crate::fsatomic::replace(
            &self.path,
            &serde_json::to_vec(&self.status).expect("usage status serializes"),
            crate::fsatomic::Staging::new(".claude-usage-status"),
            crate::fsatomic::Durability::FsyncFileAndDir,
        ) {
            let cause = error.to_string();
            if self.persistence_error.as_ref() != Some(&cause) {
                tracing::error!("Cannot persist Claude usage observation status: {error}");
            }
            self.persistence_error = Some(cause);
        } else {
            self.persistence_error = None;
        }
    }

    fn record(&mut self, result: Result<()>) -> bool {
        let fresh = match result {
            Ok(()) => {
                let changed = !self.initialized || !self.status.available;
                if !self.status.errors.is_empty() {
                    tracing::warn!(errors = ?self.status.errors, "Claude usage observation recovered; retained usage gaps still require audit");
                }
                self.status.available = true;
                self.status.errors.clear();
                changed
            }
            Err(error) => {
                let cause = format!("{error:#}");
                let count = self.status.errors.entry(cause.clone()).or_default();
                *count = count.saturating_add(1);
                let first = *count == 1;
                if first {
                    tracing::error!(
                        count = *count,
                        "Claude native usage observation unavailable: {cause}"
                    );
                }
                self.status.available = false;
                first
            }
        };
        if fresh {
            self.persist();
        }
        self.initialized = true;
        // Repeat counts stay in memory while a fault is unchanged; flush on shutdown/new cause
        // and include them on recovery. No log or fsync per malformed-transcript write.
        fresh
    }
}

pub(crate) fn warning(agent: &Path, incarnation: &str) -> Option<String> {
    let bytes = match fs::read(agent.join("claude-usage-observation.json")) {
        Ok(bytes) => bytes,
        Err(error) => {
            return Some(format!(
                "native transcript usage observation status unavailable: {error}; Stop-time usage alone does not prove complete acquisition"
            ));
        }
    };
    let status: ObservationStatus = match serde_json::from_slice(&bytes) {
        Ok(status) => status,
        Err(error) => return Some(format!("invalid native usage observation status: {error}")),
    };
    if status.incarnation != incarnation {
        return Some("no native usage observation status for the current incarnation".into());
    }
    (!status.available || !status.errors.is_empty()).then(|| format!("native transcript usage observation unavailable: {:?}; repair the reported watch/source/cursor cause; retain source and cursor evidence before operator repair, then re-adopt the wrapper; never delete a cursor to replay historical spending", status.errors))
}

pub(crate) fn supervise<T>(
    paths: &Paths,
    identity: &str,
    runtime: &str,
    incarnation: &str,
    provider: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let reporter = Arc::new(Mutex::new(Reporter::new(&paths.agent_dir, incarnation)));
    let progress = reporter.clone();
    let started = Observer::start_reporting(paths, identity, runtime, incarnation, move |result| {
        progress.lock().unwrap().record(result);
    });
    supervise_result(started, &reporter, provider)
}

fn supervise_result<T>(
    started: Result<Observer>,
    reporter: &Arc<Mutex<Reporter>>,
    provider: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let usage = match started {
        Ok(observer) => Some(observer),
        Err(error) => {
            reporter.lock().unwrap().record(Err(error));
            None
        }
    };
    let outcome = provider();
    drop(usage);
    reporter.lock().unwrap().persist();
    outcome
}

pub(crate) struct Observer {
    events: Events,
    worker: Option<JoinHandle<()>>,
}

impl Observer {
    fn start_reporting(
        paths: &Paths,
        identity: &str,
        runtime: &str,
        incarnation: &str,
        report: impl Fn(Result<()>) + Send + 'static,
    ) -> Result<Self> {
        let config = std::env::var_os("CLAUDE_CONFIG_DIR")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|x| PathBuf::from(x).join(".claude")))
            .context("Claude usage observer has no configured native store")?;
        Self::start_in(paths, identity, runtime, incarnation, &config, report)
    }

    fn start_in(
        paths: &Paths,
        identity: &str,
        runtime: &str,
        incarnation: &str,
        config: &Path,
        report: impl Fn(Result<()>) + Send + 'static,
    ) -> Result<Self> {
        fs::create_dir_all(&paths.agent_dir)?;
        fs::create_dir_all(&paths.session_dir)?;
        fs::create_dir_all(config.join("projects"))?;
        let projects = config.join("projects").canonicalize()?;
        let (send, receive) = mpsc::sync_channel(1);
        let events = Events {
            send,
            filter: Arc::new(Mutex::new(EventFilter {
                bindings: vec![
                    future_path(&paths.agent_dir.join("claude-native-session"))?,
                    future_path(&paths.session_dir.join("binding.json"))?,
                    future_path(&paths.session_dir.join("binding.pending.json"))?,
                ],
                transcript: None,
            })),
            pending: Arc::new(Mutex::new(Changes::default())),
        };
        let callback = events.clone();
        let mut watch = RecommendedWatcher::new(
            move |event| callback.record(event),
            notify::Config::default().with_follow_symlinks(false),
        )?;
        let mut watched = BTreeMap::new();
        refresh_watches(&mut watch, &mut watched, paths, &projects, None)?;
        let paths = paths.clone();
        let identity = identity.to_owned();
        let runtime = runtime.to_owned();
        let incarnation = incarnation.to_owned();
        let pending = events.pending.clone();
        let filter = events.filter.clone();
        let worker = thread::Builder::new()
            .name("claude-usage".into())
            .spawn(move || {
                let mut bound: Option<PathBuf> = None;
                let mut writer = Writer::new(&paths.agent_dir, "claude", &incarnation)
                    .with_account(crate::account::claude_account_cached(&paths.agent_dir));
                let mut reconcile = |watch: &mut RecommendedWatcher| -> Result<()> {
                    writer.set_account(crate::account::claude_account_cached(&paths.agent_dir));
                    // The lightweight hook advertises a not-yet-created file; only register its
                    // configured project directory here. Full SessionStart validation below owns
                    // publication and the incarnation fence.
                    let hook_bytes = match fs::read(paths.agent_dir.join("claude-native-session")) {
                        Ok(raw) => Some(raw),
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                        Err(error) => return Err(error.into()),
                    };
                    if let Some(raw) = hook_bytes {
                        let hook: Value = serde_json::from_slice(&raw)?;
                        if hook["incarnation"].as_str() == Some(&incarnation)
                            && let Some(raw_path) = hook["transcript_path"].as_str()
                        {
                            let path = future_path(Path::new(raw_path))?;
                            let parent = path
                                .parent()
                                .context("Claude hook transcript has no parent")?;
                            anyhow::ensure!(
                                parent.starts_with(&projects),
                                "Claude hook advertised an outside project"
                            );
                            filter.lock().unwrap().transcript = Some(path.clone());
                            refresh_watches(watch, &mut watched, &paths, &projects, Some(&path))?;
                            if !parent.is_dir() {
                                return Ok(());
                            }
                            let parent = parent.canonicalize()?;
                            anyhow::ensure!(
                                parent.starts_with(&projects),
                                "Claude hook advertised an outside project"
                            );
                            if !path.exists() {
                                return Ok(());
                            }
                        }
                    }
                    if let Some(path) = crate::claude_session::channel_transcript_recovering(
                        &paths,
                        &identity,
                        &runtime,
                        &incarnation,
                    )? {
                        let path = path.canonicalize()?;
                        filter.lock().unwrap().transcript = Some(path.clone());
                        let parent = path
                            .parent()
                            .context("Claude usage transcript has no parent")?
                            .canonicalize()?;
                        anyhow::ensure!(
                            parent.starts_with(&projects),
                            "Claude binding advertised an outside project"
                        );
                        refresh_watches(watch, &mut watched, &paths, &projects, Some(&path))?;
                        if bound.as_ref().is_some_and(|old| old != &path) {
                            drain(
                                &mut writer,
                                &paths.agent_dir,
                                bound.as_ref().unwrap(),
                                &projects,
                            )?;
                        }
                        drain(&mut writer, &paths.agent_dir, &path, &projects)?;
                        bound = Some(path);
                    }
                    Ok(())
                };
                report(reconcile(&mut watch));
                while let Ok(wake) = receive.recv() {
                    match wake {
                        Wake::Stop => {
                            report(reconcile(&mut watch));
                            break;
                        }
                        Wake::Changed => {
                            let changes = std::mem::take(&mut *pending.lock().unwrap());
                            if changes.errors.is_empty() {
                                if changes.changed {
                                    report(reconcile(&mut watch));
                                }
                            } else {
                                for error in changes.errors {
                                    report(Err(anyhow::anyhow!(error)));
                                }
                                // An overflow can lose any transcript event. Reconcile the exact
                                // bound files now, retain the failure until a valid later event,
                                // and keep the existing subscription alive instead of dying.
                                if let Err(error) = reconcile(&mut watch) {
                                    report(Err(error));
                                }
                            }
                        }
                    }
                }
            })?;
        Ok(Self {
            events,
            worker: Some(worker),
        })
    }
}

impl Drop for Observer {
    fn drop(&mut self) {
        let _ = self.events.send.send(Wake::Stop);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write as _;
    use std::time::Duration;

    const SESSION: &str = "11111111-2222-4333-8444-555555555555";

    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf, Writer) {
        let root = tempfile::tempdir().unwrap();
        let agent = root.path().join("observations");
        let projects = root.path().join("native/projects");
        let project = projects.join("workspace");
        fs::create_dir_all(&agent).unwrap();
        fs::create_dir_all(&project).unwrap();
        let agent = agent.canonicalize().unwrap();
        let projects = projects.canonicalize().unwrap();
        let project = project.canonicalize().unwrap();
        let path = project.join(format!("{SESSION}.jsonl"));
        fs::write(&path, b"").unwrap();
        let writer =
            Writer::new(&agent, "claude", "wrapper").with_account(Some("account/test".into()));
        (root, agent, projects, path, writer)
    }

    fn append(path: &Path, value: Value) {
        let mut f = fs::OpenOptions::new().append(true).open(path).unwrap();
        writeln!(f, "{value}").unwrap();
    }

    fn response(id: &str, tokens: u64, done: bool) -> Value {
        json!({"type":"assistant", "parentUuid":"not-a-user-turn", "sessionId":SESSION,
            "message":{"id":id,"model":"claude-test", "stop_reason":if done {Some("end_turn")} else {None},
            "usage":{"input_tokens":2,"output_tokens":tokens,"cache_read_input_tokens":100,
            "cache_creation_input_tokens":7,"cache_creation":{"ephemeral_1h_input_tokens":7}}}})
    }

    fn usages(agent: &Path) -> Vec<crate::harness_timeline::Operation> {
        crate::harness_timeline::read(&crate::harness_timeline::timeline_path(agent))
            .unwrap()
            .operations
            .into_iter()
            .filter(|x| x.entry_type == "usage")
            .collect()
    }

    #[test]
    fn final_written_after_stop_is_observed_without_another_turn() {
        let (_root, agent, projects, path, mut writer) = fixture();
        append(&path, json!({"type":"user","uuid":"turn-a"}));
        fs::write(
            agent.join(".harness-timeline-claude-cursor"),
            serde_json::to_vec(
                &json!({"session_id":SESSION,"offset":path.metadata().unwrap().len()}),
            )
            .unwrap(),
        )
        .unwrap();
        append(&path, response("late-final", 11, true));
        drain(&mut writer, &agent, &path, &projects).unwrap();
        let rows = usages(&agent);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].body["turn_id"], "turn-a");
        assert_eq!(rows[0].body["output_tokens"], 11);
        assert_eq!(rows[0].body["cache_write_1h_tokens"], 7);
        drain(&mut writer, &agent, &path, &projects).unwrap();
        assert_eq!(usages(&agent).len(), 1);
    }

    #[test]
    fn streaming_waits_for_completion_and_keeps_first_turn_across_reads() {
        let (_root, agent, projects, path, mut writer) = fixture();
        append(&path, json!({"type":"user","uuid":"turn-a"}));
        append(&path, response("stream", 3, false));
        drain(&mut writer, &agent, &path, &projects).unwrap();
        assert!(!crate::harness_timeline::timeline_path(&agent).exists());
        append(&path, json!({"type":"user","uuid":"turn-b"}));
        append(&path, response("stream", 17, true));
        drain(&mut writer, &agent, &path, &projects).unwrap();
        let rows = usages(&agent);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].body["turn_id"], "turn-a");
        assert_eq!(rows[0].body["output_tokens"], 17);
        append(
            &path,
            json!({"type":"user","uuid":"tool-result","message":{"content":[{"type":"tool_result"}]}}),
        );
        append(&path, response("next", 4, true));
        drain(&mut writer, &agent, &path, &projects).unwrap();
        assert_eq!(usages(&agent)[1].body["turn_id"], "turn-b");
    }

    #[test]
    fn stop_between_stream_writes_cannot_freeze_provisional_usage() {
        let (_root, agent, projects, path, mut writer) = fixture();
        append(&path, json!({"type":"user","uuid":"streaming-turn"}));
        append(&path, response("shared-stream", 3, false));
        drain(&mut writer, &agent, &path, &projects).unwrap();
        crate::harness_timeline::observe_claude_stop_transcript(
            &mut writer,
            &json!({"session_id":SESSION,"transcript_path":path}),
            projects.parent().unwrap(),
        )
        .unwrap();
        assert!(!crate::harness_timeline::timeline_path(&agent).exists());
        append(&path, response("shared-stream", 29, true));
        drain(&mut writer, &agent, &path, &projects).unwrap();
        let rows = usages(&agent);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].body["output_tokens"], 29);
        assert_eq!(rows[0].body["turn_id"], "streaming-turn");
        crate::harness_timeline::observe_claude_stop_transcript(
            &mut writer,
            &json!({"session_id":SESSION,"transcript_path":path}),
            projects.parent().unwrap(),
        )
        .unwrap();
        assert_eq!(usages(&agent).len(), 1);
    }

    #[test]
    fn partial_json_does_not_advance_the_cursor_and_bad_complete_json_is_an_error() {
        let (_root, agent, projects, path, mut writer) = fixture();
        append(&path, json!({"type":"user","uuid":"turn-a"}));
        let offset = path.metadata().unwrap().len();
        let bytes = response("split", 8, true).to_string();
        let cut = bytes.len() / 2;
        let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(&bytes.as_bytes()[..cut]).unwrap();
        drain(&mut writer, &agent, &path, &projects).unwrap();
        let cursor: Cursor =
            serde_json::from_slice(&fs::read(agent.join(".claude-usage-cursor.json")).unwrap())
                .unwrap();
        assert_eq!(cursor.offset, offset);
        f.write_all(&bytes.as_bytes()[cut..]).unwrap();
        writeln!(f).unwrap();
        drain(&mut writer, &agent, &path, &projects).unwrap();
        assert_eq!(usages(&agent).len(), 1);
        writeln!(f, "bad json").unwrap();
        assert!(drain(&mut writer, &agent, &path, &projects).is_err());
    }

    #[test]
    fn migration_does_not_replay_already_consumed_history() {
        let (_root, agent, projects, path, mut writer) = fixture();
        append(&path, json!({"type":"user","uuid":"old-turn"}));
        append(&path, response("already-consumed", 1000, true));
        fs::write(
            agent.join(".harness-timeline-claude-cursor"),
            serde_json::to_vec(
                &json!({"session_id":SESSION,"offset":path.metadata().unwrap().len()}),
            )
            .unwrap(),
        )
        .unwrap();
        append(&path, response("late-final", 9, true));
        drain(&mut writer, &agent, &path, &projects).unwrap();
        let rows = usages(&agent);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].body["output_tokens"], 9);
    }

    #[test]
    fn filesystem_append_wakes_the_observer_without_stop_or_timer() {
        let (root, agent, projects, path, _writer) = fixture();
        let session_dir = root.path().join("session");
        fs::create_dir_all(&session_dir).unwrap();
        let paths = Paths {
            root: root.path().to_owned(),
            agent_dir: agent.clone(),
            session_dir: session_dir.clone(),
        };
        fs::write(session_dir.join("binding.json"), serde_json::to_vec(&json!({
            "schema":"st.claude-session-binding.v1","agent":"host.worker","runtimeId":"runtime",
            "runtimeIncarnation":"wrapper","nativeSessionId":SESSION,"canonicalWorkspace":root.path(),"transcriptPath":path
        })).unwrap()).unwrap();
        let (sent, received) = mpsc::channel();
        let observer = Observer::start_in(
            &paths,
            "host.worker",
            "runtime",
            "wrapper",
            projects.parent().unwrap(),
            move |r| {
                sent.send(r.map_err(|e| e.to_string())).unwrap();
            },
        )
        .unwrap();
        received
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        append(&path, json!({"type":"user","uuid":"event-turn"}));
        append(&path, response("event-final", 13, true));
        // The event can arrive between the two writes. The callback carries completed
        // acquisition, so wait on events rather than repeatedly reading the transcript.
        loop {
            received
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap();
            if crate::harness_timeline::timeline_path(&agent).exists() {
                break;
            }
        }
        assert_eq!(usages(&agent)[0].body["output_tokens"], 13);
        drop(observer);
    }
    #[test]
    fn outside_native_store_and_truncated_transcript_fail_explicitly() {
        let (root, agent, projects, path, mut writer) = fixture();
        let outside = root.path().join("outside.jsonl");
        fs::write(&outside, b"").unwrap();
        assert!(drain(&mut writer, &agent, &outside, &projects).is_err());
        append(&path, json!({"type":"user","uuid":"turn-a"}));
        append(&path, response("first", 5, true));
        drain(&mut writer, &agent, &path, &projects).unwrap();
        fs::write(&path, b"").unwrap();
        assert!(
            drain(&mut writer, &agent, &path, &projects)
                .unwrap_err()
                .to_string()
                .contains("shrank")
        );
    }

    #[test]
    fn a_foreign_wrapper_binding_cannot_publish_usage() {
        let (root, agent, projects, path, _writer) = fixture();
        let session_dir = root.path().join("session");
        fs::create_dir_all(&session_dir).unwrap();
        let paths = Paths {
            root: root.path().to_owned(),
            agent_dir: agent.clone(),
            session_dir: session_dir.clone(),
        };
        fs::write(session_dir.join("binding.json"),serde_json::to_vec(&json!({
            "schema":"st.claude-session-binding.v1","agent":"host.worker","runtimeId":"runtime",
            "runtimeIncarnation":"other-wrapper","nativeSessionId":SESSION,"canonicalWorkspace":root.path(),"transcriptPath":path
        })).unwrap()).unwrap();
        append(&path, json!({"type":"user","uuid":"foreign-turn"}));
        append(&path, response("foreign-final", 99, true));
        let (sent, received) = mpsc::channel();
        let observer = Observer::start_in(
            &paths,
            "host.worker",
            "runtime",
            "wrapper",
            projects.parent().unwrap(),
            move |r| {
                sent.send(r.map_err(|e| e.to_string())).unwrap();
            },
        )
        .unwrap();
        received
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        assert!(!crate::harness_timeline::timeline_path(&agent).exists());
        drop(observer);
        assert!(!crate::harness_timeline::timeline_path(&agent).exists());
    }
    #[test]
    fn subagent_completion_is_captured_without_the_parent_stop() {
        let (_root, agent, projects, path, mut writer) = fixture();
        let children = path.parent().unwrap().join(SESSION).join("subagents");
        fs::create_dir_all(&children).unwrap();
        let child = children.join("agent-child1.jsonl");
        fs::write(&child, b"").unwrap();
        append(&child, response("child-stream", 2, false));
        drain(&mut writer, &agent, &path, &projects).unwrap();
        assert!(!crate::harness_timeline::timeline_path(&agent).exists());
        append(&child, response("child-stream", 21, true));
        drain(&mut writer, &agent, &path, &projects).unwrap();
        let rows = usages(&agent);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].body["turn_id"], "subagent:child1");
        assert_eq!(rows[0].body["output_tokens"], 21);
        drain(&mut writer, &agent, &path, &projects).unwrap();
        assert_eq!(usages(&agent).len(), 1);
    }
    #[test]
    fn subagent_directory_symlink_cannot_leave_the_configured_store() {
        let (root, agent, projects, path, mut writer) = fixture();
        let outside = root.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        let session_dir = path.parent().unwrap().join(SESSION);
        fs::create_dir_all(&session_dir).unwrap();
        std::os::unix::fs::symlink(&outside, session_dir.join("subagents")).unwrap();
        assert!(
            drain(&mut writer, &agent, &path, &projects)
                .unwrap_err()
                .to_string()
                .contains("outside")
        );
    }
    #[test]
    fn malformed_legacy_subagent_cursor_is_not_replayed_from_zero() {
        let (_root, agent, projects, path, mut writer) = fixture();
        let children = path.parent().unwrap().join(SESSION).join("subagents");
        fs::create_dir_all(&children).unwrap();
        let child = children.join("agent-child1.jsonl");
        fs::write(&child, b"").unwrap();
        append(&child, response("old-child", 99, true));
        fs::write(
            agent.join(".harness-timeline-claude-subagent-cursors"),
            serde_json::to_vec(
                &json!({"session_id":SESSION,"offsets":{"agent-child1.jsonl":"invalid"}}),
            )
            .unwrap(),
        )
        .unwrap();
        assert!(drain(&mut writer, &agent, &path, &projects).is_err());
        assert!(!crate::harness_timeline::timeline_path(&agent).exists());
    }

    fn event_observer(
        root: &Path,
        agent: &Path,
        projects: &Path,
        path: &Path,
    ) -> (Observer, mpsc::Receiver<std::result::Result<(), String>>) {
        let paths = Paths {
            root: root.to_owned(),
            agent_dir: agent.to_owned(),
            session_dir: root.join("session"),
        };
        fs::create_dir_all(&paths.session_dir).unwrap();
        fs::write(paths.session_dir.join("binding.json"), serde_json::to_vec(&json!({
            "schema":"st.claude-session-binding.v1", "agent":"host.worker", "runtimeId":"runtime",
            "runtimeIncarnation":"wrapper", "nativeSessionId":SESSION,
            "canonicalWorkspace":root, "transcriptPath":path
        })).unwrap()).unwrap();
        let (sent, received) = mpsc::channel();
        let observer = Observer::start_in(
            &paths,
            "host.worker",
            "runtime",
            "wrapper",
            projects.parent().unwrap(),
            move |result| {
                let _ = sent.send(result.map_err(|error| error.to_string()));
            },
        )
        .unwrap();
        received
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        (observer, received)
    }

    #[test]
    fn start_failure_preserves_provider_execution_and_stop_usage() {
        let (root, agent, projects, path, mut writer) = fixture();
        let invalid_config = root.path().join("not-a-directory");
        fs::write(&invalid_config, b"not a native config directory").unwrap();
        let paths = Paths {
            root: root.path().into(),
            agent_dir: agent.clone(),
            session_dir: root.path().join("session"),
        };
        let started = Observer::start_in(
            &paths,
            "host.worker",
            "runtime",
            "wrapper",
            &invalid_config,
            |_| {},
        );
        assert!(started.is_err());
        let reporter = Arc::new(Mutex::new(Reporter::new(&agent, "wrapper")));
        append(
            &path,
            json!({"type":"user","uuid":"turn-after-watch-failure"}),
        );
        append(&path, response("stop-after-watch-failure", 15, true));
        // Both launch and adoption run through this same accounting/supervision boundary.
        let launched = supervise_result(started, &reporter, || {
            let status = std::process::Command::new("sh")
                .args(["-c", "exit 0"])
                .status()?;
            anyhow::ensure!(status.success(), "provider control did not launch");
            crate::harness_timeline::observe_claude_stop_transcript(
                &mut writer,
                &json!({"session_id":SESSION,"transcript_path":path}),
                projects.parent().unwrap(),
            )?;
            Ok(42)
        })
        .unwrap();
        assert_eq!(launched, 42);
        assert_eq!(usages(&agent)[0].body["output_tokens"], 15);
        assert!(warning(&agent, "wrapper").unwrap().contains("unavailable"));
    }

    #[test]
    fn notify_error_rescans_and_keeps_observing_later_writes() {
        let (root, agent, projects, path, _writer) = fixture();
        let (observer, received) = event_observer(root.path(), &agent, &projects, &path);
        observer
            .events
            .record(Err(notify::Error::generic("test queue overflow")));
        assert!(
            received
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap_err()
                .contains("overflow")
        );
        append(&path, json!({"type":"user","uuid":"turn-after-overflow"}));
        append(&path, response("after-overflow", 19, true));
        loop {
            received
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap();
            if crate::harness_timeline::timeline_path(&agent).exists() {
                break;
            }
        }
        assert_eq!(usages(&agent)[0].body["output_tokens"], 19);
        drop(observer);
    }

    #[test]
    fn unrelated_session_write_does_not_reconcile_this_observer() {
        let (root, agent, projects, path, _writer) = fixture();
        let (observer, received) = event_observer(root.path(), &agent, &projects, &path);
        fs::write(
            path.parent().unwrap().join("another-session.jsonl"),
            b"other seat\n",
        )
        .unwrap();
        assert!(matches!(
            received.recv_timeout(Duration::from_millis(150)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        assert!(!crate::harness_timeline::timeline_path(&agent).exists());
        drop(observer);
    }

    #[test]
    fn streaming_events_coalesce_into_one_pending_reconciliation() {
        let (send, received) = mpsc::sync_channel(1);
        let transcript = PathBuf::from("/configured/project/session.jsonl");
        let events = Events {
            send,
            filter: Arc::new(Mutex::new(EventFilter {
                bindings: vec![],
                transcript: Some(transcript.clone()),
            })),
            pending: Arc::new(Mutex::new(Changes::default())),
        };
        for _ in 0..1000 {
            events.record(Ok(
                notify::Event::new(EventKind::Any).add_path(transcript.clone())
            ));
        }
        assert!(matches!(received.try_recv(), Ok(Wake::Changed)));
        assert!(matches!(
            received.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        assert!(events.pending.lock().unwrap().changed);
    }

    #[test]
    fn repeated_bad_line_reports_once_and_retains_counts_until_recovery() {
        let (_root, agent, projects, path, mut writer) = fixture();
        fs::write(&path, b"complete malformed line\n").unwrap();
        let mut reporter = Reporter::new(&agent, "wrapper");
        assert!(reporter.record(drain(&mut writer, &agent, &path, &projects)));
        for _ in 0..3 {
            assert!(!reporter.record(drain(&mut writer, &agent, &path, &projects)));
        }
        reporter.persist();
        let status: ObservationStatus =
            serde_json::from_slice(&fs::read(&reporter.path).unwrap()).unwrap();
        assert_eq!(status.errors.values().copied().sum::<u64>(), 4);
        assert!(warning(&agent, "wrapper").is_some());
        fs::write(&path, b"").unwrap();
        append(&path, json!({"type":"user","uuid":"recovered-turn"}));
        append(&path, response("recovered", 7, true));
        reporter.record(drain(&mut writer, &agent, &path, &projects));
        assert!(warning(&agent, "wrapper").is_none());
        assert!(warning(&agent, "different-wrapper").is_some());
        assert_eq!(usages(&agent)[0].body["output_tokens"], 7);
    }

    #[test]
    fn subagent_directory_created_after_start_is_observed_through_its_parent() {
        let (root, agent, projects, path, _writer) = fixture();
        let (observer, received) = event_observer(root.path(), &agent, &projects, &path);
        let children = path.parent().unwrap().join(SESSION).join("subagents");
        fs::create_dir_all(&children).unwrap();
        received
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        let child = children.join("agent-new.jsonl");
        fs::write(&child, b"").unwrap();
        append(&child, response("new-child", 23, true));
        loop {
            received
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap();
            if crate::harness_timeline::timeline_path(&agent).exists() {
                break;
            }
        }
        assert_eq!(usages(&agent)[0].body["output_tokens"], 23);
        drop(observer);
    }

    #[test]
    fn native_session_rebinding_retires_watches_and_recreated_directories_are_rearmed() {
        let (root, agent, projects, path, _writer) = fixture();
        let paths = Paths {
            root: root.path().into(),
            agent_dir: agent,
            session_dir: root.path().join("session"),
        };
        fs::create_dir_all(&paths.session_dir).unwrap();
        let mut watch = RecommendedWatcher::new(|_| {}, notify::Config::default()).unwrap();
        let mut watched = BTreeMap::new();
        let children = path.parent().unwrap().join(SESSION).join("subagents");
        fs::create_dir_all(&children).unwrap();
        refresh_watches(&mut watch, &mut watched, &paths, &projects, Some(&path)).unwrap();
        assert_eq!(watched.len(), 6);
        let old_identity = watched[&children];
        fs::rename(&children, children.with_file_name("retired-subagents")).unwrap();
        fs::create_dir(&children).unwrap();
        refresh_watches(&mut watch, &mut watched, &paths, &projects, Some(&path)).unwrap();
        assert_ne!(watched[&children], old_identity);
        let next = path.parent().unwrap().join("next-session.jsonl");
        fs::create_dir_all(next.parent().unwrap().join("next-session/subagents")).unwrap();
        refresh_watches(&mut watch, &mut watched, &paths, &projects, Some(&next)).unwrap();
        assert_eq!(watched.len(), 6);
        assert!(!watched.contains_key(&children));
    }
}
