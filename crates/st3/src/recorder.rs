//! Records every `git` and `gh` call that st3 starts, then runs the real program unchanged.
//!
//! `st3 up` links `git` and `gh` in `<state>/recorder/bin` to its own executable and puts that
//! directory first on the PATH of the daemon and of every member it starts. When st3 starts under
//! one of those names, it runs the next program of that name on PATH that is not a recorder, waits
//! for it, appends one JSON line to `<state>/recorder/commands.jsonl`, and exits the way the real
//! program exited.
//!
//! A recorder observes; it does not enforce. A program called by its absolute path skips it, so a
//! quiet log does not prove that nothing ran. A failure to record never fails, delays, or changes
//! the command.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::Write as _;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

/// The programs st3 records.
pub const PROGRAMS: [&str; 2] = ["git", "gh"];

/// Every recorder directory holds this file. A lookup for the real program skips any directory
/// that has one, so a recorder never runs itself or another recorder.
const MARKER: &str = "st3-recorder.json";
const MARKER_SCHEMA: &str = "st3.recorder.v1";
const RECORD_SCHEMA: &str = "st3.recorder.command.v1";
/// `execvp` searches this list when PATH is unset.
const DEFAULT_PATH: &str = "/usr/bin:/bin";
const ARGUMENT_LIMIT: usize = 4096;

#[derive(Debug, Deserialize, Serialize)]
struct Marker {
    schema: String,
    host: String,
    log: PathBuf,
}

/// The recorder directory and log that `install` prepared.
#[derive(Clone, Debug)]
pub struct Installation {
    pub directory: PathBuf,
    pub log: PathBuf,
    pub programs: Vec<&'static str>,
}

/// The absolute recorder directory for a state directory. PATH entries must not depend on the
/// working directory of the process that reads them.
pub fn directory(state_dir: &Path) -> Result<PathBuf> {
    Ok(std::path::absolute(state_dir.join("recorder").join("bin"))?)
}

/// The append-only command log for a state directory.
pub fn log_path(state_dir: &Path) -> Result<PathBuf> {
    Ok(std::path::absolute(
        state_dir.join("recorder").join("commands.jsonl"),
    )?)
}

/// Puts the recorder directory first on a PATH and removes its other occurrences.
pub fn prepend(directory: &Path, path: Option<&OsStr>) -> Result<OsString> {
    let rest = std::env::split_paths(path.unwrap_or(OsStr::new(DEFAULT_PATH)))
        .filter(|entry| entry != directory)
        .collect::<Vec<_>>();
    std::env::join_paths(std::iter::once(directory.to_path_buf()).chain(rest))
        .context("join the recorder directory with PATH")
}

/// Links each program that resolves on one of the search paths to `executable`, and removes the
/// link of a program that does not, so `command -v gh` still fails on a host without `gh`.
pub fn install(
    state_dir: &Path,
    host: &str,
    executable: &Path,
    search_paths: &[&OsStr],
) -> Result<Installation> {
    let directory = directory(state_dir)?;
    let log = log_path(state_dir)?;
    fs::create_dir_all(&directory)
        .with_context(|| format!("create the recorder directory {}", directory.display()))?;
    let marker = serde_json::to_vec_pretty(&Marker {
        schema: MARKER_SCHEMA.into(),
        host: host.into(),
        log: log.clone(),
    })?;
    replace_file(&directory.join(MARKER), &marker)?;
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&log)
        .with_context(|| format!("open the command log {}", log.display()))?;
    let own = fs::metadata(executable).ok();
    let mut programs = Vec::new();
    for program in PROGRAMS {
        let link = directory.join(program);
        let available = search_paths
            .iter()
            .any(|path| real_program(program, Some(path), own.as_ref()).is_some());
        if available {
            replace_link(executable, &link)?;
            programs.push(program);
        } else if fs::symlink_metadata(&link).is_ok() {
            fs::remove_file(&link)
                .with_context(|| format!("remove the recorder link {}", link.display()))?;
        }
    }
    Ok(Installation {
        directory,
        log,
        programs,
    })
}

/// Whether the daemon's recorder can record, and a sentence that says what it records or why not.
pub fn health(state_dir: &Path) -> (bool, String) {
    let (Ok(directory), Ok(log)) = (directory(state_dir), log_path(state_dir)) else {
        return (false, "the recorder directory cannot be resolved".into());
    };
    if !directory.join(MARKER).is_file() {
        return (
            false,
            format!(
                "{} has no recorder, so no git or gh call is recorded",
                directory.display()
            ),
        );
    }
    let linked = PROGRAMS
        .into_iter()
        .filter(|program| is_executable(&directory.join(program)))
        .collect::<Vec<_>>();
    if linked.is_empty() {
        return (
            false,
            format!("{} links neither git nor gh", directory.display()),
        );
    }
    if let Err(error) = fs::OpenOptions::new()
        .append(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&log)
    {
        return (
            false,
            format!(
                "the command log {} cannot be appended: {error}",
                log.display()
            ),
        );
    }
    (
        true,
        format!(
            "{} calls are recorded in {}; a call by absolute path is not",
            linked.join(" and "),
            log.display()
        ),
    )
}

/// The recorded program when this process started under a recorder name.
pub fn invoked_program() -> Option<&'static str> {
    let argv0 = std::env::args_os().next()?;
    let name = Path::new(&argv0).file_name()?;
    PROGRAMS
        .into_iter()
        .find(|program| name == OsStr::new(program))
}

/// Runs the real program, records the call, and exits the way the real program exited.
pub fn run(program: &'static str) -> ! {
    let started_at = SystemTime::now();
    let started = Instant::now();
    let mut arguments = std::env::args_os();
    let argv0 = arguments.next().unwrap_or_else(|| program.into());
    let arguments = arguments.collect::<Vec<_>>();
    let path = std::env::var_os("PATH");
    let marker = own_marker(program, &argv0, path.as_deref());
    let own = std::env::current_exe()
        .ok()
        .and_then(|executable| fs::metadata(executable).ok());
    let real = real_program(program, path.as_deref(), own.as_ref());
    let outcome = match &real {
        // A bare name stays bare, as a shell would pass it; a path names the real program.
        Some(real) if argv0.as_bytes().contains(&b'/') => relay(real, real.as_os_str(), &arguments),
        Some(real) => relay(real, &argv0, &arguments),
        None => {
            eprintln!("st3 recorder: {program}: command not found after the recorder on PATH");
            Outcome::Exited(127)
        }
    };
    if let Some(marker) = marker {
        let call = Call {
            program,
            arguments: &arguments,
            real: real.as_deref(),
            outcome: &outcome,
            started_at,
            duration: started.elapsed(),
        };
        append(&marker, &call);
    }
    outcome.finish()
}

/// The recorder directory this process was started from. The first executable of this name on
/// PATH is the one `execvp` ran; a name with a slash names its directory directly.
fn own_marker(program: &str, argv0: &OsStr, path: Option<&OsStr>) -> Option<Marker> {
    let directory = if argv0.as_bytes().contains(&b'/') {
        Path::new(argv0).parent()?.to_path_buf()
    } else {
        std::env::split_paths(path.unwrap_or(OsStr::new(DEFAULT_PATH)))
            .find(|entry| is_executable(&search_entry(entry).join(program)))?
    };
    let bytes = fs::read(search_entry(&directory).join(MARKER)).ok()?;
    let marker = serde_json::from_slice::<Marker>(&bytes).ok()?;
    (marker.schema == MARKER_SCHEMA).then_some(marker)
}

/// The first executable of this name on PATH that is neither in a recorder directory nor the
/// running executable itself.
fn real_program(
    program: &str,
    path: Option<&OsStr>,
    own: Option<&fs::Metadata>,
) -> Option<PathBuf> {
    std::env::split_paths(path.unwrap_or(OsStr::new(DEFAULT_PATH))).find_map(|entry| {
        let entry = search_entry(&entry);
        if entry.join(MARKER).exists() {
            return None;
        }
        let candidate = entry.join(program);
        let metadata = fs::metadata(&candidate).ok()?;
        let executable = metadata.is_file() && metadata.permissions().mode() & 0o111 != 0;
        let itself =
            own.is_some_and(|own| own.dev() == metadata.dev() && own.ino() == metadata.ino());
        (executable && !itself).then_some(candidate)
    })
}

/// An empty PATH entry means the working directory. The result always contains a slash, so
/// running it never searches PATH again.
fn search_entry(entry: &Path) -> PathBuf {
    if entry.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        entry.to_path_buf()
    }
}

fn is_executable(path: &Path) -> bool {
    fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

fn replace_file(path: &Path, bytes: &[u8]) -> Result<()> {
    if fs::read(path).is_ok_and(|current| current == bytes) {
        return Ok(());
    }
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    fs::write(&temporary, bytes)
        .with_context(|| format!("write the recorder marker {}", temporary.display()))?;
    fs::rename(&temporary, path)
        .with_context(|| format!("replace the recorder marker {}", path.display()))
}

fn replace_link(executable: &Path, link: &Path) -> Result<()> {
    if fs::read_link(link).is_ok_and(|target| target == executable) {
        return Ok(());
    }
    let temporary = link.with_extension(format!("tmp-{}", std::process::id()));
    let _ = fs::remove_file(&temporary);
    std::os::unix::fs::symlink(executable, &temporary)
        .with_context(|| format!("link {} to {}", temporary.display(), executable.display()))?;
    fs::rename(&temporary, link)
        .with_context(|| format!("replace the recorder link {}", link.display()))
}

#[derive(Debug)]
enum Outcome {
    Exited(i32),
    Signaled(i32),
}

impl Outcome {
    fn from_status(status: ExitStatus) -> Self {
        match (status.code(), status.signal()) {
            (Some(code), _) => Self::Exited(code),
            (None, Some(signal)) => Self::Signaled(signal),
            (None, None) => Self::Exited(1),
        }
    }

    /// Ends this process with the real program's status. A signal is raised again, so a shell
    /// that stops a loop on an interrupted child sees an interrupted child here too.
    fn finish(self) -> ! {
        match self {
            Self::Exited(code) => std::process::exit(code),
            Self::Signaled(signal) => {
                unsafe {
                    // The real program may have dumped core; this process has nothing to add.
                    let limit = libc::rlimit {
                        rlim_cur: 0,
                        rlim_max: 0,
                    };
                    libc::setrlimit(libc::RLIMIT_CORE, &limit);
                    libc::signal(signal, libc::SIG_DFL);
                    let mut set = std::mem::zeroed::<libc::sigset_t>();
                    libc::sigemptyset(&mut set);
                    libc::sigaddset(&mut set, signal);
                    libc::sigprocmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut());
                    libc::raise(signal);
                }
                std::process::exit(128 + signal)
            }
        }
    }
}

/// Signals a caller can send this process on purpose. Each one reaches the real program. The
/// terminal's own SIGINT, SIGQUIT, and SIGWINCH already reach it through the shared process
/// group, so Linux does not send them twice. Stop and continue signals keep their default action
/// and apply to the whole process group.
const RELAYED: [libc::c_int; 8] = [
    libc::SIGHUP,
    libc::SIGINT,
    libc::SIGQUIT,
    libc::SIGTERM,
    libc::SIGUSR1,
    libc::SIGUSR2,
    libc::SIGALRM,
    libc::SIGWINCH,
];

/// The write end of the pipe that carries each received signal number to the wait loop.
static WAKE: AtomicI32 = AtomicI32::new(-1);

extern "C" fn note_signal(
    signal: libc::c_int,
    info: *mut libc::siginfo_t,
    _context: *mut libc::c_void,
) {
    if signal != libc::SIGCHLD && !sent_by_a_process(info) {
        return;
    }
    unsafe {
        let errno = errno_location();
        let saved = *errno;
        let byte = signal as u8;
        libc::write(WAKE.load(Ordering::Relaxed), (&raw const byte).cast(), 1);
        *errno = saved;
    }
}

/// Linux gives signals from kill(2), sigqueue(3), and tgkill(2) a code at or below zero, and a
/// signal from the terminal driver a positive one.
#[cfg(target_os = "linux")]
fn sent_by_a_process(info: *const libc::siginfo_t) -> bool {
    info.is_null() || unsafe { (*info).si_code } <= 0
}

/// Other hosts relay every signal. The real program can then see a terminal interrupt twice.
#[cfg(not(target_os = "linux"))]
fn sent_by_a_process(_info: *const libc::siginfo_t) -> bool {
    true
}

#[cfg(target_os = "linux")]
unsafe fn errno_location() -> *mut libc::c_int {
    unsafe { libc::__errno_location() }
}

#[cfg(not(target_os = "linux"))]
unsafe fn errno_location() -> *mut libc::c_int {
    unsafe { libc::__error() }
}

/// Runs the real program as a child in this process group with this process's stdio, working
/// directory, and environment, and relays signals until it exits.
fn relay(real: &Path, argv0: &OsStr, arguments: &[OsString]) -> Outcome {
    let mut command = std::process::Command::new(real);
    command.arg0(argv0).args(arguments);
    let mut pipe = [-1; 2];
    if unsafe { libc::pipe(pipe.as_mut_ptr()) } != 0 {
        return spawn_unrelayed(command, real);
    }
    let [read_end, write_end] = pipe;
    unsafe {
        libc::fcntl(read_end, libc::F_SETFD, libc::FD_CLOEXEC);
        libc::fcntl(write_end, libc::F_SETFD, libc::FD_CLOEXEC);
        libc::fcntl(write_end, libc::F_SETFL, libc::O_NONBLOCK);
    }
    WAKE.store(write_end, Ordering::Relaxed);

    // Hold every handled signal until the child's pid is known. The child restores the caller's
    // mask and dispositions before it runs the real program.
    let mut original_mask = unsafe { std::mem::zeroed::<libc::sigset_t>() };
    let mut previous = Vec::new();
    unsafe {
        let mut handled = std::mem::zeroed::<libc::sigset_t>();
        libc::sigemptyset(&mut handled);
        for signal in RELAYED.into_iter().chain([libc::SIGCHLD]) {
            libc::sigaddset(&mut handled, signal);
        }
        libc::sigprocmask(libc::SIG_BLOCK, &handled, &mut original_mask);
        for signal in RELAYED.into_iter().chain([libc::SIGCHLD]) {
            let mut current = std::mem::zeroed::<libc::sigaction>();
            libc::sigaction(signal, std::ptr::null(), &mut current);
            // An ignored signal stays ignored, and the real program inherits that.
            if signal != libc::SIGCHLD && current.sa_sigaction == libc::SIG_IGN {
                continue;
            }
            let mut action = std::mem::zeroed::<libc::sigaction>();
            action.sa_sigaction = note_signal as *const () as libc::sighandler_t;
            action.sa_flags = libc::SA_SIGINFO | libc::SA_RESTART;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(signal, &action, std::ptr::null_mut());
            previous.push((signal, current));
        }
    }
    let parent = std::process::id() as libc::pid_t;
    unsafe {
        command.pre_exec(move || {
            for (signal, action) in &previous {
                libc::sigaction(*signal, action, std::ptr::null_mut());
            }
            // Killing this process outright takes the real program with it, as it would have
            // ended the real program without a recorder.
            #[cfg(target_os = "linux")]
            {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong);
                if libc::getppid() != parent {
                    return Err(std::io::Error::other(
                        "the recorder exited before its child",
                    ));
                }
            }
            #[cfg(not(target_os = "linux"))]
            let _ = parent;
            libc::sigprocmask(libc::SIG_SETMASK, &original_mask, std::ptr::null_mut());
            Ok(())
        });
    }
    let spawned = command.spawn();
    unsafe {
        libc::sigprocmask(libc::SIG_SETMASK, &original_mask, std::ptr::null_mut());
    }
    let mut child = match spawned {
        Ok(child) => child,
        Err(error) => return spawn_failure(real, &error),
    };
    let pid = child.id() as libc::pid_t;
    let mut signals = [0_u8; 64];
    loop {
        // Only this loop reaps the child, and it signals the pid only before reaping it, so a
        // relayed signal never reaches a reused pid.
        match child.try_wait() {
            Ok(Some(status)) => return Outcome::from_status(status),
            Ok(None) => {}
            Err(_) => break,
        }
        let count = unsafe { libc::read(read_end, signals.as_mut_ptr().cast(), signals.len()) };
        if count < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        for signal in &signals[..count as usize] {
            let signal = libc::c_int::from(*signal);
            if signal != libc::SIGCHLD {
                unsafe {
                    libc::kill(pid, signal);
                }
            }
        }
    }
    match child.wait() {
        Ok(status) => Outcome::from_status(status),
        Err(_) => Outcome::Exited(1),
    }
}

/// Without a signal pipe, the real program still runs; signals then reach only this process.
fn spawn_unrelayed(mut command: std::process::Command, real: &Path) -> Outcome {
    match command.status() {
        Ok(status) => Outcome::from_status(status),
        Err(error) => spawn_failure(real, &error),
    }
}

fn spawn_failure(real: &Path, error: &std::io::Error) -> Outcome {
    eprintln!("st3 recorder: cannot run {}: {error}", real.display());
    if error.kind() == std::io::ErrorKind::NotFound {
        Outcome::Exited(127)
    } else {
        Outcome::Exited(126)
    }
}

struct Call<'a> {
    program: &'a str,
    arguments: &'a [OsString],
    real: Option<&'a Path>,
    outcome: &'a Outcome,
    started_at: SystemTime,
    duration: Duration,
}

#[derive(Serialize)]
struct Record<'a> {
    schema: &'static str,
    time: String,
    host: &'a str,
    actor: String,
    subject: Option<String>,
    step_run: Option<String>,
    cwd: Option<String>,
    program: &'a str,
    args: Vec<String>,
    real: Option<String>,
    exit_code: Option<i32>,
    signal: Option<i32>,
    duration_ms: f64,
}

fn record<'a>(marker: &'a Marker, call: &Call<'a>) -> Record<'a> {
    let variable = |name| {
        std::env::var_os(name)
            .filter(|value| !value.is_empty())
            .map(|value| value.to_string_lossy().into_owned())
    };
    let subject = variable("ST3_SUBJECT");
    let actor = variable("ST_AGENT")
        .or_else(|| subject.clone())
        .unwrap_or_else(|| "daemon".into());
    let (exit_code, signal) = match call.outcome {
        Outcome::Exited(code) => (Some(*code), None),
        Outcome::Signaled(signal) => (None, Some(*signal)),
    };
    Record {
        schema: RECORD_SCHEMA,
        time: chrono::DateTime::<chrono::Utc>::from(call.started_at)
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        host: &marker.host,
        actor,
        subject,
        step_run: variable("ST_STEP_RUN"),
        cwd: std::env::current_dir()
            .ok()
            .map(|cwd| cwd.to_string_lossy().into_owned()),
        program: call.program,
        args: call
            .arguments
            .iter()
            .map(|argument| describe_argument(argument))
            .collect(),
        real: call.real.map(|real| real.to_string_lossy().into_owned()),
        exit_code,
        signal,
        duration_ms: call.duration.as_micros() as f64 / 1000.0,
    }
}

/// Appends one line. Every failure is silent: the log is for later analysis, and the command's
/// own output and status must not change because of it. O_NONBLOCK keeps a FIFO without a reader
/// from holding the command open.
fn append(marker: &Marker, call: &Call<'_>) {
    let Ok(mut line) = serde_json::to_vec(&record(marker, call)) else {
        return;
    };
    line.push(b'\n');
    let Ok(mut file) = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NONBLOCK)
        .open(&marker.log)
    else {
        return;
    };
    let _ = file.write_all(&line);
}

/// Arguments are recorded as text with URL credentials, extra HTTP headers, and authorization
/// headers replaced, and each is cut to a bounded length. This is a best effort; the log is
/// private to the account.
fn describe_argument(argument: &OsStr) -> String {
    let text = redact(&argument.to_string_lossy());
    if text.len() <= ARGUMENT_LIMIT {
        return text;
    }
    let mut end = ARGUMENT_LIMIT;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…[{} more bytes]", &text[..end], text.len() - end)
}

fn redact(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    if lower.starts_with("authorization:") {
        return "Authorization: ***".into();
    }
    if let Some(index) = lower.find("extraheader=") {
        let end = index + "extraheader=".len();
        return format!("{}***", &text[..end]);
    }
    let mut redacted = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(index) = rest.find("://") {
        let authority_start = index + 3;
        redacted.push_str(&rest[..authority_start]);
        rest = &rest[authority_start..];
        let authority_end = rest
            .find(|character: char| {
                matches!(character, '/' | '?' | '#') || character.is_whitespace()
            })
            .unwrap_or(rest.len());
        if let Some(at) = rest[..authority_end].rfind('@') {
            redacted.push_str("***");
            rest = &rest[at..];
        }
    }
    redacted.push_str(rest);
    redacted
}

#[cfg(test)]
mod tests {
    use super::*;

    fn executable(path: &Path) {
        fs::write(path, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn the_recorder_directory_leads_path_once() {
        let path = prepend(
            Path::new("/state/recorder/bin"),
            Some(OsStr::new("/usr/bin:/state/recorder/bin::/bin")),
        )
        .unwrap();
        assert_eq!(path, "/state/recorder/bin:/usr/bin::/bin");
        assert_eq!(
            prepend(Path::new("/state/recorder/bin"), None).unwrap(),
            "/state/recorder/bin:/usr/bin:/bin"
        );
    }

    #[test]
    fn a_relative_state_directory_gives_an_absolute_recorder_directory() {
        let directory = directory(Path::new("relative-state")).unwrap();
        assert!(directory.is_absolute());
        assert!(directory.ends_with("relative-state/recorder/bin"));
        assert!(log_path(Path::new("relative-state")).unwrap().is_absolute());
    }

    #[test]
    fn the_real_program_skips_recorder_directories_and_this_executable() {
        let root = tempfile::tempdir().unwrap();
        let recorder = root.path().join("recorder");
        let unmarked = root.path().join("unmarked");
        let real = root.path().join("real");
        for directory in [&recorder, &unmarked, &real] {
            fs::create_dir(directory).unwrap();
            executable(&directory.join("git"));
        }
        fs::write(recorder.join(MARKER), "{}").unwrap();
        fs::remove_file(unmarked.join("git")).unwrap();
        fs::hard_link(recorder.join("git"), unmarked.join("git")).unwrap();
        let own = fs::metadata(recorder.join("git")).unwrap();
        let path = std::env::join_paths([&recorder, &unmarked, &real]).unwrap();

        assert_eq!(
            real_program("git", Some(&path), Some(&own)),
            Some(real.join("git"))
        );
        let only_recorders = std::env::join_paths([&recorder, &unmarked]).unwrap();
        assert_eq!(real_program("git", Some(&only_recorders), Some(&own)), None);
    }

    #[test]
    fn a_program_that_is_not_executable_is_not_the_real_program() {
        let root = tempfile::tempdir().unwrap();
        let first = root.path().join("first");
        let second = root.path().join("second");
        fs::create_dir(&first).unwrap();
        fs::create_dir(&second).unwrap();
        fs::write(first.join("gh"), "not a program").unwrap();
        fs::create_dir(first.join("git")).unwrap();
        executable(&second.join("gh"));
        executable(&second.join("git"));
        let path = std::env::join_paths([&first, &second]).unwrap();

        assert_eq!(
            real_program("gh", Some(&path), None),
            Some(second.join("gh"))
        );
        assert_eq!(
            real_program("git", Some(&path), None),
            Some(second.join("git"))
        );
    }

    #[test]
    fn an_empty_path_entry_names_the_working_directory_with_a_slash() {
        assert_eq!(search_entry(Path::new("")).join("git"), Path::new("./git"));
    }

    #[test]
    fn install_links_only_programs_that_resolve() {
        let root = tempfile::tempdir().unwrap();
        let tools = root.path().join("tools");
        fs::create_dir(&tools).unwrap();
        executable(&tools.join("git"));
        let st3 = root.path().join("st3");
        executable(&st3);
        let state = root.path().join("state");
        fs::create_dir_all(state.join("recorder/bin")).unwrap();
        std::os::unix::fs::symlink(&st3, state.join("recorder/bin/gh")).unwrap();

        let installation = install(&state, "test-host", &st3, &[tools.as_os_str()]).unwrap();

        assert_eq!(installation.programs, ["git"]);
        assert_eq!(
            fs::read_link(installation.directory.join("git")).unwrap(),
            st3
        );
        assert!(fs::symlink_metadata(installation.directory.join("gh")).is_err());
        let marker: Marker =
            serde_json::from_slice(&fs::read(installation.directory.join(MARKER)).unwrap())
                .unwrap();
        assert_eq!(marker.host, "test-host");
        assert_eq!(marker.log, installation.log);
        assert_eq!(
            fs::metadata(&installation.log)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let again = install(&state, "test-host", &st3, &[tools.as_os_str()]).unwrap();
        assert_eq!(again.programs, ["git"]);
    }

    #[test]
    fn health_says_whether_calls_are_recorded() {
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("state");
        let (recording, message) = health(&state);
        assert!(!recording);
        assert!(message.contains("has no recorder"), "{message}");

        let tools = root.path().join("tools");
        fs::create_dir(&tools).unwrap();
        executable(&tools.join("gh"));
        let st3 = root.path().join("st3");
        executable(&st3);
        let installation = install(&state, "test-host", &st3, &[tools.as_os_str()]).unwrap();
        let (recording, message) = health(&state);
        assert!(recording, "{message}");
        assert!(
            message.starts_with(&format!(
                "gh calls are recorded in {}",
                installation.log.display()
            )),
            "{message}"
        );

        fs::remove_file(installation.directory.join("gh")).unwrap();
        let (recording, message) = health(&state);
        assert!(!recording);
        assert!(message.contains("links neither git nor gh"), "{message}");
    }

    #[test]
    fn credentials_in_arguments_are_redacted() {
        assert_eq!(
            redact("https://x-access-token:secret@github.example/org/repo.git"),
            "https://***@github.example/org/repo.git"
        );
        assert_eq!(
            redact("remote.origin.url=https://token@host.example"),
            "remote.origin.url=https://***@host.example"
        );
        assert_eq!(
            redact("http.https://host.example/.extraheader=AUTHORIZATION: basic c2VjcmV0"),
            "http.https://host.example/.extraheader=***"
        );
        assert_eq!(redact("Authorization: token secret"), "Authorization: ***");
        assert_eq!(
            redact("https://host.example/a@b"),
            "https://host.example/a@b"
        );
        assert_eq!(redact("commit message"), "commit message");
    }

    #[test]
    fn long_arguments_are_cut_on_a_character_boundary() {
        let long = "é".repeat(ARGUMENT_LIMIT);
        let described = describe_argument(OsStr::new(&long));
        assert!(described.len() < ARGUMENT_LIMIT + 32);
        assert!(described.ends_with(&format!("…[{} more bytes]", ARGUMENT_LIMIT)));
    }
}
