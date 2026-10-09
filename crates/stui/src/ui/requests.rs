//! Requests from a seat to the terminal UI on the same machine: open an agent, a mission or a
//! machine in a tab or a split, and focus it or leave the person's focus where it is.
//!
//! `st ui open` writes one small file into a private directory for the configured person and
//! waits for the answer; every `st` terminal UI of that person on this machine reads the
//! directory, shows the pane and writes the answer. Nothing goes through the graph, so no
//! existing daemon, phone or terminal build has anything new to understand. A request is a
//! picture of what to show, never an action on someone's behalf: it opens panes the person can
//! close, and it names no text of its own.

use super::glass_store::person_state;
use super::pane::Pane;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const VERSION: u32 = 1;
const MAX_BYTES: u64 = 4096;
/// More than this waiting in the directory is a runaway writer: the rest are left alone.
const MAX_FILES: usize = 64;
const MAX_SUBJECT: usize = 256;
/// A request nobody picked up in this long is stale: the person is not looking at a terminal UI.
pub const MAX_AGE: Duration = Duration::from_secs(10 * 60);
/// How long a terminal UI waits for a just-made agent to reach its inventory before it answers
/// that the subject does not exist.
pub const GRACE: Duration = Duration::from_secs(6);
/// How often the directory is read.
const EVERY: Duration = Duration::from_millis(300);

/// Where the new pane goes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Place {
    /// A new tab in the focused split.
    #[default]
    Tab,
    /// A new split to the right of the focused one.
    Right,
    /// A new split below the focused one.
    Below,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub version: u32,
    /// The file's name without its extension; time ordered.
    #[serde(skip)]
    pub id: String,
    /// What to show: `agent/…`, `mission/…`, `mission-run/…` or `machine/…`.
    pub subject: String,
    #[serde(default)]
    pub place: Place,
    /// Show the pane without moving the person's focus to it.
    #[serde(default)]
    pub keep_focus: bool,
    /// The seat that asked, for the line that tells the person why the screen changed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    pub at_unix_ms: u64,
}

/// What the terminal UI answered.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Answer {
    /// It is on screen. `focused` says whether the person's focus moved to it.
    Opened { focused: bool },
    /// st has no such agent, mission or machine.
    NotFound,
}

/// The pane a subject names, or why it cannot be asked for.
pub fn pane_for(subject: &str) -> Result<Pane, String> {
    if subject.is_empty()
        || subject.len() > MAX_SUBJECT
        || subject.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err("name an agent, mission or machine without spaces".into());
    }
    if let Some(run) = subject.strip_prefix("mission-run/") {
        // `mission-run/NAME/RUN` is a run of `mission/NAME`; the mission view follows its runs.
        let name = run.rsplit_once('/').map_or(run, |(name, _)| name);
        if name.is_empty() {
            return Err("a mission run is mission-run/NAME/RUN".into());
        }
        return Ok(Pane::Mission(Some(format!("mission/{name}"))));
    }
    for (prefix, make) in [
        ("agent/", Pane::Agent as fn(Option<String>) -> Pane),
        ("mission/", Pane::Mission),
        ("machine/", Pane::Machine),
    ] {
        if subject.len() > prefix.len() && subject.starts_with(prefix) {
            return Ok(make(Some(subject.to_owned())));
        }
    }
    Err("st ui open shows agent/…, mission/…, mission-run/… or machine/…".into())
}

/// Where this person's requests wait on this device.
pub fn dir(person: &str) -> Option<PathBuf> {
    let (state, key) = person_state(person)?;
    Some(state.join(format!("requests-{key}")))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}

fn mine(metadata: &fs::Metadata) -> bool {
    // SAFETY: geteuid has no preconditions.
    metadata.uid() == unsafe { libc::geteuid() } && metadata.permissions().mode() & 0o077 == 0
}

/// The directory, made private to this user when it is new; `None` when it is not theirs alone.
fn private_dir(dir: &Path, create: bool) -> Option<()> {
    if create {
        fs::create_dir_all(dir).ok()?;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).ok()?;
    }
    let metadata = fs::symlink_metadata(dir).ok()?;
    (metadata.is_dir() && mine(&metadata)).then_some(())
}

fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let temp = path.with_extension(format!("{}.tmp", uuid::Uuid::now_v7()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(&temp, path).inspect_err(|_| {
        let _ = fs::remove_file(&temp);
    })
}

/// Queue a request; returns its id.
pub fn enqueue(dir: &Path, subject: &str, place: Place, keep_focus: bool, from: Option<String>) -> std::io::Result<String> {
    private_dir(dir, true).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("{} is not private to this user", dir.display()),
        )
    })?;
    let id = uuid::Uuid::now_v7().to_string();
    let request = Request {
        version: VERSION,
        id: String::new(),
        subject: subject.to_owned(),
        place,
        keep_focus,
        from,
        at_unix_ms: now_ms(),
    };
    write_private(&dir.join(format!("{id}.json")), &serde_json::to_vec(&request)?)?;
    Ok(id)
}

/// Wait for the terminal UI's answer to request `id`. `None` when no terminal UI answered in
/// time; the request is then withdrawn, so nothing opens later that the seat was told failed.
pub fn wait(dir: &Path, id: &str, timeout: Duration) -> Option<Answer> {
    let started = Instant::now();
    let answer = dir.join(format!("{id}.answer"));
    loop {
        if let Some(found) = read_answer(&answer) {
            let _ = fs::remove_file(&answer);
            return Some(found);
        }
        if started.elapsed() >= timeout {
            let _ = fs::remove_file(dir.join(format!("{id}.json")));
            // An answer written while the request was being withdrawn still counts.
            let found = read_answer(&answer);
            let _ = fs::remove_file(&answer);
            return found;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn read_answer(path: &Path) -> Option<Answer> {
    let metadata = fs::symlink_metadata(path).ok()?;
    if !metadata.is_file() || !mine(&metadata) || metadata.len() > MAX_BYTES {
        return None;
    }
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

/// The requests a terminal UI has read and not yet answered.
pub struct Inbox {
    dir: Option<PathBuf>,
    seen: HashSet<String>,
    waiting: Vec<(Request, Instant)>,
    read_at: Option<Instant>,
}

impl Inbox {
    pub fn new(dir: Option<PathBuf>) -> Self {
        Self {
            dir,
            seen: HashSet::new(),
            waiting: Vec::new(),
            read_at: None,
        }
    }

    /// Read new requests, at most every few hundred milliseconds.
    pub fn read(&mut self) {
        let Some(dir) = self.dir.clone() else { return };
        if self.read_at.is_some_and(|at| at.elapsed() < EVERY) {
            return;
        }
        self.read_at = Some(Instant::now());
        if private_dir(&dir, false).is_none() {
            return;
        }
        let Ok(entries) = fs::read_dir(&dir) else {
            return;
        };
        let mut names = entries
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        names.sort();
        let now = now_ms();
        for name in names.into_iter().take(MAX_FILES) {
            let path = dir.join(&name);
            let Some(id) = name.strip_suffix(".json") else {
                // Answers nobody collected and half-written files age out.
                if let Ok(metadata) = fs::symlink_metadata(&path)
                    && metadata
                        .modified()
                        .ok()
                        .and_then(|at| at.elapsed().ok())
                        .is_some_and(|age| age > MAX_AGE)
                {
                    let _ = fs::remove_file(&path);
                }
                continue;
            };
            if self.seen.contains(id) {
                continue;
            }
            let Some(mut request) = fs::symlink_metadata(&path)
                .ok()
                .filter(|metadata| metadata.is_file() && mine(metadata) && metadata.len() <= MAX_BYTES)
                .and_then(|_| fs::read(&path).ok())
                .and_then(|bytes| serde_json::from_slice::<Request>(&bytes).ok())
                .filter(|request| request.version == VERSION && pane_for(&request.subject).is_ok())
            else {
                let _ = fs::remove_file(&path);
                continue;
            };
            if now.saturating_sub(request.at_unix_ms) > MAX_AGE.as_millis() as u64 {
                let _ = fs::remove_file(&path);
                continue;
            }
            request.id = id.to_owned();
            self.seen.insert(id.to_owned());
            self.waiting.push((request, Instant::now()));
        }
        // A withdrawn request is forgotten, and so is its id.
        self.waiting.retain(|(request, _)| dir.join(format!("{}.json", request.id)).exists());
        self.seen.retain(|id| dir.join(format!("{id}.json")).exists());
    }

    /// The requests waiting, oldest first, for the caller to open or leave.
    pub fn waiting(&self) -> impl Iterator<Item = &(Request, Instant)> {
        self.waiting.iter()
    }

    /// Make a waiting request look as if it had waited `by` already.
    #[cfg(test)]
    pub fn waiting_since_for_test(&mut self, id: &str, by: Duration) {
        for (request, since) in &mut self.waiting {
            if request.id == id {
                *since = Instant::now().checked_sub(by + Duration::from_millis(1)).unwrap();
            }
        }
    }

    /// Answer a request and take it off the queue.
    pub fn answer(&mut self, id: &str, answer: &Answer) {
        self.waiting.retain(|(request, _)| request.id != id);
        let Some(dir) = self.dir.as_deref() else { return };
        if let Ok(bytes) = serde_json::to_vec(answer) {
            let _ = write_private(&dir.join(format!("{id}.answer")), &bytes);
        }
        let _ = fs::remove_file(dir.join(format!("{id}.json")));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn private(path: &Path) -> PathBuf {
        let dir = path.join("requests");
        fs::create_dir_all(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }

    #[test]
    fn subjects_name_the_panes_a_seat_may_ask_for() {
        let pane = |subject: &str| pane_for(subject).map(|pane| pane.key());
        assert_eq!(pane("agent/garden/worker"), Ok("agent:agent/garden/worker".into()));
        assert_eq!(pane("mission/garden/notes"), Ok("mission:mission/garden/notes".into()));
        assert_eq!(pane("machine/harbor"), Ok("machine:machine/harbor".into()));
        // A run opens the mission view that follows all of that mission's runs.
        assert_eq!(
            pane("mission-run/garden/notes/2026-10-08"),
            Ok("mission:mission/garden/notes".into())
        );
        assert_eq!(pane("mission-run/solo"), Ok("mission:mission/solo".into()));
        for refused in [
            "",
            "agent/",
            "attention/one",
            "person/ada",
            "agent/with space",
            "agent/with\nnewline",
            "mission-run/",
        ] {
            assert!(pane(refused).is_err(), "{refused:?}");
        }
        assert!(pane(&format!("agent/{}", "a".repeat(MAX_SUBJECT))).is_err());
    }

    #[test]
    fn a_request_is_read_once_answered_and_collected() {
        let root = tempfile::tempdir().unwrap();
        let dir = private(root.path());
        let id = enqueue(&dir, "agent/garden/worker", Place::Right, true, Some("agent/st/assistant".into())).unwrap();
        assert!(fs::metadata(dir.join(format!("{id}.json"))).unwrap().permissions().mode() & 0o077 == 0);

        let mut inbox = Inbox::new(Some(dir.clone()));
        inbox.read();
        let waiting = inbox.waiting().map(|(request, _)| request.clone()).collect::<Vec<_>>();
        assert_eq!(waiting.len(), 1);
        assert_eq!(waiting[0].id, id);
        assert_eq!(waiting[0].place, Place::Right);
        assert!(waiting[0].keep_focus);
        assert_eq!(waiting[0].from.as_deref(), Some("agent/st/assistant"));

        // Read again later: still one, not two.
        inbox.read_at = None;
        inbox.read();
        assert_eq!(inbox.waiting().count(), 1);

        inbox.answer(&id, &Answer::Opened { focused: false });
        assert_eq!(inbox.waiting().count(), 0);
        assert!(!dir.join(format!("{id}.json")).exists());
        assert_eq!(wait(&dir, &id, Duration::from_secs(1)), Some(Answer::Opened { focused: false }));
        assert!(!dir.join(format!("{id}.answer")).exists());
    }

    #[test]
    fn a_request_nobody_answers_is_withdrawn_when_the_seat_stops_waiting() {
        let root = tempfile::tempdir().unwrap();
        let dir = private(root.path());
        let id = enqueue(&dir, "mission/garden/notes", Place::Tab, false, None).unwrap();
        assert_eq!(wait(&dir, &id, Duration::from_millis(120)), None);
        let mut inbox = Inbox::new(Some(dir.clone()));
        inbox.read();
        assert_eq!(inbox.waiting().count(), 0, "withdrawn, so it opens nothing later");
    }

    #[test]
    fn stale_unreadable_and_unsafe_requests_are_dropped() {
        let root = tempfile::tempdir().unwrap();
        let dir = private(root.path());
        let write = |name: &str, body: &str, mode: u32| {
            let path = dir.join(name);
            fs::write(&path, body).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
            path
        };
        let stale = serde_json::json!({"version": 1, "subject": "agent/a/b", "at_unix_ms": 1});
        let fresh = serde_json::json!({"version": 1, "subject": "agent/a/b", "at_unix_ms": now_ms()});
        let bad_subject = serde_json::json!({"version": 1, "subject": "attention/x", "at_unix_ms": now_ms()});
        let future = serde_json::json!({"version": 2, "subject": "agent/a/b", "at_unix_ms": now_ms()});
        let stale = write("0001.json", &stale.to_string(), 0o600);
        let junk = write("0002.json", "not json", 0o600);
        let bad = write("0003.json", &bad_subject.to_string(), 0o600);
        let open = write("0004.json", &fresh.to_string(), 0o644);
        let newer = write("0005.json", &future.to_string(), 0o600);
        let good = write("0006.json", &fresh.to_string(), 0o600);
        let huge = write("0007.json", &"x".repeat(MAX_BYTES as usize + 1), 0o600);

        let mut inbox = Inbox::new(Some(dir.clone()));
        inbox.read();
        let ids = inbox.waiting().map(|(request, _)| request.id.clone()).collect::<Vec<_>>();
        assert_eq!(ids, ["0006"]);
        for gone in [stale, junk, bad, open, newer, huge] {
            assert!(!gone.exists(), "{}", gone.display());
        }
        assert!(good.exists());
    }

    #[test]
    fn a_directory_other_users_can_write_to_is_not_read() {
        let root = tempfile::tempdir().unwrap();
        let dir = private(root.path());
        enqueue(&dir, "agent/a/b", Place::Tab, false, None).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o770)).unwrap();
        let mut inbox = Inbox::new(Some(dir.clone()));
        inbox.read();
        assert_eq!(inbox.waiting().count(), 0);
        assert!(enqueue(&dir, "agent/a/b", Place::Tab, false, None).is_ok(), "enqueue repairs its own directory's mode");
    }

    #[test]
    fn no_person_means_no_directory() {
        assert_eq!(dir(""), None);
        let mut inbox = Inbox::new(None);
        inbox.read();
        assert_eq!(inbox.waiting().count(), 0);
    }
}
