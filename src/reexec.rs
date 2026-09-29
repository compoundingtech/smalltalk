//! Re-execute a long-lived seat process into a replaced st binary without ending its session.
//!
//! A deploy installs a new st binary at the same path and restarts the daemon. The processes that
//! carry a seat's messages (the native driver, the Claude channel, and the pi-family channel) run
//! for the whole provider session, so without help they keep executing the deleted binary and talk
//! to the new daemon with old code. Each of them watches the installed binary. Once a replacement
//! is complete and answers the resume probe, the process writes its live state to a private file,
//! keeps the descriptors its provider still needs, and calls `execve` on the new binary. `execve`
//! keeps the PID, the children, the terminal, and every descriptor without close-on-exec, so the
//! provider never notices. The new image reads the state back and adopts the running provider.
//!
//! The stop signals are blocked across the exec. A stop that arrives in that window stays pending
//! until the new image has installed its handlers, so it is never lost and never kills the process
//! with the default action.

use std::ffi::{CString, OsStr, OsString};
use std::fs;
use std::io::{self, Read as _, Write as _};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::os::unix::io::RawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// Names the state file a re-executed native driver resumes from.
pub const DRIVER_RESUME_ENV: &str = "ST3_DRIVER_RESUME";
/// Names the state file a re-executed Claude or pi-family channel resumes from.
pub const CHANNEL_RESUME_ENV: &str = "ST3_CHANNEL_RESUME";

/// The resume state format this build writes. A replacement binary must list it in its probe
/// answer before an older process executes it, so a rollback to a build that cannot read the state
/// keeps the current code running instead of losing the session.
pub const RESUME_FORMAT: u32 = 1;
/// Every resume state format this build can read.
pub const READABLE_RESUME_FORMATS: &[u32] = &[1];
/// The first word of the probe answer.
pub const PROBE_BANNER: &str = "st3-resume";
/// The hidden subcommand a replacement binary answers the probe on.
pub const PROBE_SUBCOMMAND: &str = "resume-probe";

/// A replacement must look the same for this long before it is executed. `mv` installs a complete
/// file at once; a copy in place is still being written while its size or time changes.
const STABLE_FOR: Duration = Duration::from_secs(1);
/// A replacement that failed its probe is retried after this long.
const REFUSED_RETRY: Duration = Duration::from_secs(30);
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// The identity of an executable file: its inode and the size and time it was written with.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageIdentity {
    pub dev: u64,
    pub ino: u64,
    pub size: u64,
    pub mtime_ns: i128,
}

impl ImageIdentity {
    /// The identity of the file at `path`, following symbolic links.
    pub fn of(path: &Path) -> io::Result<Self> {
        let metadata = fs::metadata(path)?;
        Ok(Self {
            dev: metadata.dev(),
            ino: metadata.ino(),
            size: metadata.size(),
            mtime_ns: i128::from(metadata.mtime()) * 1_000_000_000
                + i128::from(metadata.mtime_nsec()),
        })
    }

    /// The identity of the image this process executes. Linux reads it through `/proc/self/exe`,
    /// which still names the running inode after the file was replaced.
    pub fn running() -> io::Result<Self> {
        #[cfg(target_os = "linux")]
        {
            Self::of(Path::new("/proc/self/exe"))
        }
        #[cfg(not(target_os = "linux"))]
        {
            Self::of(&std::env::current_exe()?)
        }
    }

    /// A compact spelling for headers and logs.
    pub fn token(&self) -> String {
        format!("{}:{}:{}:{}", self.dev, self.ino, self.size, self.mtime_ns)
    }
}

/// The identity of this process's image, read once.
pub fn running_identity() -> Option<ImageIdentity> {
    static RUNNING: OnceLock<Option<ImageIdentity>> = OnceLock::new();
    *RUNNING.get_or_init(|| ImageIdentity::running().ok())
}

/// The installed st binary this process should follow: `ST3_BIN` when it names a file, otherwise
/// the path this process was started from. Linux spells an unlinked image `PATH (deleted)`.
pub fn installed_binary() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("ST3_BIN").filter(|value| !value.is_empty()) {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Some(path);
        }
    }
    let current = std::env::current_exe().ok()?;
    let spelled = current.as_os_str().as_bytes();
    let original = spelled
        .strip_suffix(b" (deleted)")
        .map(|bytes| PathBuf::from(OsStr::from_bytes(bytes)))
        .unwrap_or(current);
    Some(original)
}

/// Watches the installed binary for a replacement this process can execute.
pub struct ReplacementWatch {
    path: PathBuf,
    running: ImageIdentity,
    sighted: Option<(ImageIdentity, Instant)>,
    refused: Option<(ImageIdentity, Instant)>,
}

impl ReplacementWatch {
    /// Watch the binary [`installed_binary`] names.
    pub fn for_current_process() -> Option<Self> {
        Self::new(installed_binary()?)
    }

    pub fn new(path: PathBuf) -> Option<Self> {
        Some(Self {
            path,
            running: running_identity()?,
            sighted: None,
            refused: None,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether the installed file is no longer the image this process runs.
    pub fn stale(&self) -> bool {
        ImageIdentity::of(&self.path).is_ok_and(|installed| installed != self.running)
    }

    /// Check the installed binary. Returns its path once a replacement has stayed unchanged for a
    /// second and answered the probe with this build's resume format.
    pub fn ready(&mut self) -> Option<PathBuf> {
        self.ready_after(STABLE_FOR)
    }

    fn ready_after(&mut self, stable_for: Duration) -> Option<PathBuf> {
        let installed = ImageIdentity::of(&self.path).ok()?;
        if installed == self.running || !is_executable(&self.path) {
            self.sighted = None;
            return None;
        }
        let now = Instant::now();
        if self
            .refused
            .is_some_and(|(refused, at)| refused == installed && now < at + REFUSED_RETRY)
        {
            return None;
        }
        match self.sighted {
            Some((sighted, since)) if sighted == installed => {
                if now.duration_since(since) < stable_for {
                    return None;
                }
            }
            _ => {
                self.sighted = Some((installed, now));
                if !stable_for.is_zero() {
                    return None;
                }
            }
        }
        match probe(&self.path) {
            Ok(formats) if formats.contains(&RESUME_FORMAT) => Some(self.path.clone()),
            Ok(_) | Err(_) => {
                self.refused = Some((installed, now));
                self.sighted = None;
                None
            }
        }
    }

    /// Remember that executing this replacement failed, so it is retried only after a pause.
    pub fn refuse_current(&mut self) {
        if let Ok(installed) = ImageIdentity::of(&self.path) {
            self.refused = Some((installed, Instant::now()));
        }
        self.sighted = None;
    }
}

fn is_executable(path: &Path) -> bool {
    fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

/// The probe answer: `st3-resume 1 2 ...`, listing the resume formats a build reads.
pub fn probe_answer() -> String {
    let formats = READABLE_RESUME_FORMATS
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(" ");
    format!("{PROBE_BANNER} {formats}")
}

/// Ask `binary` which resume formats it reads. A binary that predates the probe fails it.
pub fn probe(binary: &Path) -> Result<Vec<u32>> {
    let mut child = Command::new(binary)
        .arg(PROBE_SUBCOMMAND)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .env_remove(DRIVER_RESUME_ENV)
        .env_remove(CHANNEL_RESUME_ENV)
        .spawn()
        .with_context(|| format!("probing {}", binary.display()))?;
    let deadline = Instant::now() + PROBE_TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("{} did not answer the resume probe", binary.display());
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let mut output = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        stdout.read_to_string(&mut output)?;
    }
    anyhow::ensure!(
        status.success(),
        "{} refused the resume probe",
        binary.display()
    );
    parse_probe_answer(&output)
        .with_context(|| format!("{} gave no resume probe answer", binary.display()))
}

fn parse_probe_answer(output: &str) -> Option<Vec<u32>> {
    let mut words = output.lines().next()?.split_whitespace();
    (words.next()? == PROBE_BANNER).then_some(())?;
    words.map(|word| word.parse().ok()).collect()
}

/// State a process hands to its next image.
#[derive(Serialize, Deserialize)]
struct Envelope<T> {
    format: u32,
    state: T,
}

/// Write `state` to a private file in `dir` for the next image to read.
pub fn write_state<T: Serialize>(dir: &Path, name: &str, state: &T) -> Result<PathBuf> {
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let path = dir.join(format!("{name}-{}-{nanos}.json", std::process::id()));
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("creating {}", path.display()))?;
    file.write_all(&serde_json::to_vec(&Envelope {
        format: RESUME_FORMAT,
        state,
    })?)?;
    file.sync_all()?;
    Ok(path)
}

/// Read and remove a state file written by [`write_state`].
pub fn read_state<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()));
    let _ = fs::remove_file(path);
    let envelope: Envelope<T> = serde_json::from_slice(&bytes?)
        .with_context(|| format!("decoding resume state {}", path.display()))?;
    anyhow::ensure!(
        READABLE_RESUME_FORMATS.contains(&envelope.format),
        "resume state format {} is not readable by this build",
        envelope.format
    );
    Ok(envelope.state)
}

static TAKEN: OnceLock<[Option<PathBuf>; 2]> = OnceLock::new();

/// Take both resume variables out of the environment so no child inherits them.
///
/// # Safety
///
/// Call this before the process starts a second thread.
pub unsafe fn take_resume_environment() {
    let mut taken = [None, None];
    for (slot, name) in taken
        .iter_mut()
        .zip([DRIVER_RESUME_ENV, CHANNEL_RESUME_ENV])
    {
        *slot = std::env::var_os(name)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);
        unsafe { std::env::remove_var(name) };
    }
    let _ = TAKEN.set(taken);
}

/// The state file this process was re-executed with, if any.
pub fn resume_path(name: &str) -> Option<PathBuf> {
    let index = match name {
        DRIVER_RESUME_ENV => 0,
        CHANNEL_RESUME_ENV => 1,
        _ => return None,
    };
    match TAKEN.get() {
        Some(taken) => taken[index].clone(),
        None => std::env::var_os(name)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from),
    }
}

const STOP_SIGNALS: [libc::c_int; 3] = [libc::SIGTERM, libc::SIGHUP, libc::SIGINT];

fn stop_signal_set() -> libc::sigset_t {
    unsafe {
        let mut set = std::mem::zeroed::<libc::sigset_t>();
        libc::sigemptyset(&mut set);
        for signal in STOP_SIGNALS {
            libc::sigaddset(&mut set, signal);
        }
        set
    }
}

/// Unblock the stop signals a predecessor blocked across its exec. Install the handlers first: a
/// pending stop is delivered as soon as this returns.
pub fn unblock_stop_signals() {
    let set = stop_signal_set();
    unsafe {
        libc::pthread_sigmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut());
    }
}

/// Replace this process image with `binary`, keeping its PID, children, and the descriptors in
/// `keep`. The new image runs with this process's arguments and environment plus `resume_env`
/// naming `state`. Returns only when `execve` fails, after restoring the signal mask and the
/// close-on-exec flags.
pub fn exec(binary: &Path, resume_env: &str, state: &Path, keep: &[RawFd]) -> io::Error {
    exec_unless_stopped(binary, resume_env, state, keep, &|| false)
}

/// [`exec`] for a process whose stop handlers record a stop in a flag. Blocking the stop signals
/// covers only the calling thread, and a handler on another thread may already have recorded a
/// stop that the next image would never see. Once the signals are blocked, `stopped` is asked;
/// when it reports a stop, the exec is abandoned with [`io::ErrorKind::Interrupted`] so this
/// image can act on it.
pub fn exec_unless_stopped(
    binary: &Path,
    resume_env: &str,
    state: &Path,
    keep: &[RawFd],
    stopped: &dyn Fn() -> bool,
) -> io::Error {
    let arguments = std::env::args_os().skip(1).collect::<Vec<_>>();
    exec_checked(binary, &arguments, resume_env, state, keep, stopped)
}

pub fn exec_with_arguments(
    binary: &Path,
    arguments: &[OsString],
    resume_env: &str,
    state: &Path,
    keep: &[RawFd],
) -> io::Error {
    exec_checked(binary, arguments, resume_env, state, keep, &|| false)
}

fn exec_checked(
    binary: &Path,
    arguments: &[OsString],
    resume_env: &str,
    state: &Path,
    keep: &[RawFd],
    stopped: &dyn Fn() -> bool,
) -> io::Error {
    let Ok(program) = CString::new(binary.as_os_str().as_bytes()) else {
        return io::Error::new(io::ErrorKind::InvalidInput, "binary path contains NUL");
    };
    let mut argv = vec![program.clone()];
    for argument in arguments {
        let Ok(argument) = CString::new(argument.as_bytes()) else {
            return io::Error::new(io::ErrorKind::InvalidInput, "argument contains NUL");
        };
        argv.push(argument);
    }
    let mut envp = Vec::new();
    for (key, value) in std::env::vars_os() {
        if key == DRIVER_RESUME_ENV || key == CHANNEL_RESUME_ENV {
            continue;
        }
        let mut entry = key.as_bytes().to_vec();
        entry.push(b'=');
        entry.extend_from_slice(value.as_bytes());
        if let Ok(entry) = CString::new(entry) {
            envp.push(entry);
        }
    }
    let mut entry = resume_env.as_bytes().to_vec();
    entry.push(b'=');
    entry.extend_from_slice(state.as_os_str().as_bytes());
    let Ok(entry) = CString::new(entry) else {
        return io::Error::new(io::ErrorKind::InvalidInput, "state path contains NUL");
    };
    envp.push(entry);
    let argv_pointers = argv
        .iter()
        .map(|argument| argument.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect::<Vec<_>>();
    let envp_pointers = envp
        .iter()
        .map(|entry| entry.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect::<Vec<_>>();

    let mut restored = Vec::new();
    for &fd in keep {
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if flags == -1 {
            let error = io::Error::last_os_error();
            restore_close_on_exec(&restored);
            return error;
        }
        if flags & libc::FD_CLOEXEC != 0 {
            if unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } == -1 {
                let error = io::Error::last_os_error();
                restore_close_on_exec(&restored);
                return error;
            }
            restored.push(fd);
        }
    }
    let set = stop_signal_set();
    let mut previous = unsafe { std::mem::zeroed::<libc::sigset_t>() };
    unsafe {
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, &mut previous);
    }
    let error = if stopped() {
        io::Error::new(io::ErrorKind::Interrupted, "a stop arrived before the exec")
    } else {
        unsafe {
            libc::execve(
                program.as_ptr(),
                argv_pointers.as_ptr(),
                envp_pointers.as_ptr(),
            );
        }
        io::Error::last_os_error()
    };
    unsafe {
        libc::pthread_sigmask(libc::SIG_SETMASK, &previous, std::ptr::null_mut());
    }
    restore_close_on_exec(&restored);
    error
}

fn restore_close_on_exec(fds: &[RawFd]) {
    for &fd in fds {
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            if flags != -1 {
                libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
            }
        }
    }
}

/// One read from standard input.
pub enum StdinChunk {
    Bytes(Vec<u8>),
    Eof,
    Failed(io::Error),
}

/// Reads standard input on a thread that can be stopped without losing a byte.
///
/// A channel that re-executes must hand every byte it already took from its stdin pipe to the next
/// image. A thread blocked in `read` cannot be interrupted safely, so this one waits in `poll` with
/// a short timeout, hands each read to `deliver` before it looks at the stop flag again, and
/// [`StdinReader::stop`] joins it. After `stop` returns, everything it read has been delivered.
pub struct StdinReader {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl StdinReader {
    pub fn spawn(mut deliver: impl FnMut(StdinChunk) -> bool + Send + 'static) -> Self {
        use std::sync::atomic::Ordering;
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = stop.clone();
        let handle = std::thread::spawn(move || {
            let mut buffer = vec![0_u8; 64 * 1024];
            while !flag.load(Ordering::SeqCst) {
                let mut descriptor = libc::pollfd {
                    fd: 0,
                    events: libc::POLLIN,
                    revents: 0,
                };
                let ready = unsafe { libc::poll(&mut descriptor, 1, 100) };
                if ready == -1 {
                    let error = io::Error::last_os_error();
                    if error.kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    deliver(StdinChunk::Failed(error));
                    return;
                }
                if ready == 0 {
                    continue;
                }
                let read = unsafe { libc::read(0, buffer.as_mut_ptr().cast(), buffer.len()) };
                if read == -1 {
                    let error = io::Error::last_os_error();
                    if matches!(
                        error.kind(),
                        io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                    ) {
                        continue;
                    }
                    deliver(StdinChunk::Failed(error));
                    return;
                }
                if read == 0 {
                    deliver(StdinChunk::Eof);
                    return;
                }
                if !deliver(StdinChunk::Bytes(buffer[..read as usize].to_vec())) {
                    return;
                }
            }
        });
        Self {
            stop,
            handle: Some(handle),
        }
    }

    /// Stop reading and wait until every byte read so far has been delivered.
    pub fn stop(mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Splits newline-delimited frames out of the bytes a [`StdinReader`] delivers.
#[derive(Default, Serialize, Deserialize)]
pub struct LineBuffer {
    pending: Vec<u8>,
}

impl LineBuffer {
    pub fn push(&mut self, bytes: &[u8]) {
        self.pending.extend_from_slice(bytes);
    }

    /// The next complete line without its newline.
    pub fn next_line(&mut self) -> Option<String> {
        let end = self.pending.iter().position(|byte| *byte == b'\n')?;
        let line = self.pending.drain(..=end).take(end).collect::<Vec<_>>();
        Some(String::from_utf8_lossy(&line).into_owned())
    }

    /// Whatever is left once the writer closed the pipe without a final newline.
    pub fn finish(&mut self) -> Option<String> {
        if self.pending.is_empty() {
            return None;
        }
        let line = std::mem::take(&mut self.pending);
        Some(String::from_utf8_lossy(&line).into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_script(path: &Path, body: &str) {
        fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn the_probe_answer_lists_every_readable_format() {
        assert_eq!(
            parse_probe_answer(&probe_answer()).unwrap(),
            READABLE_RESUME_FORMATS
        );
        assert!(parse_probe_answer("st 0.1.0").is_none());
        assert!(parse_probe_answer("st3-resume one").is_none());
    }

    #[test]
    fn a_replacement_runs_only_after_it_is_stable_and_answers_the_probe() {
        let temp = tempfile::tempdir().unwrap();
        let installed = temp.path().join("st3");
        write_script(&installed, "echo 'st3-resume 1'");
        let mut watch = ReplacementWatch {
            path: installed.clone(),
            running: ImageIdentity {
                dev: 0,
                ino: 0,
                size: 0,
                mtime_ns: 0,
            },
            sighted: None,
            refused: None,
        };
        // The first sighting only starts the stability window.
        assert_eq!(watch.ready_after(Duration::from_millis(50)), None);
        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(
            watch.ready_after(Duration::from_millis(50)),
            Some(installed)
        );
    }

    #[test]
    fn a_replacement_that_cannot_read_this_format_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        let installed = temp.path().join("st3");
        write_script(&installed, "echo 'st 0.1.0'");
        let mut watch = ReplacementWatch {
            path: installed.clone(),
            running: ImageIdentity {
                dev: 0,
                ino: 0,
                size: 0,
                mtime_ns: 0,
            },
            sighted: None,
            refused: None,
        };
        assert_eq!(watch.ready_after(Duration::ZERO), None);
        assert!(watch.refused.is_some());
        // A refused identity is not probed again inside the retry window.
        write_script(&installed, "echo 'st3-resume 1'");
        let rewritten = ImageIdentity::of(&installed).unwrap();
        watch.refused = Some((rewritten, Instant::now()));
        assert_eq!(watch.ready_after(Duration::ZERO), None);
    }

    #[test]
    fn the_running_image_is_not_its_own_replacement() {
        let running = ImageIdentity::running().unwrap();
        let mut watch = ReplacementWatch {
            path: std::env::current_exe().unwrap(),
            running,
            sighted: None,
            refused: None,
        };
        assert!(!watch.stale());
        assert_eq!(watch.ready_after(Duration::ZERO), None);
    }

    #[test]
    fn a_line_buffer_carries_a_partial_frame() {
        let mut lines = LineBuffer::default();
        lines.push(b"{\"a\":1}\n{\"b\"");
        assert_eq!(lines.next_line().as_deref(), Some("{\"a\":1}"));
        assert_eq!(lines.next_line(), None);
        let carried: LineBuffer =
            serde_json::from_slice(&serde_json::to_vec(&lines).unwrap()).unwrap();
        let mut lines = carried;
        lines.push(b":2}\n");
        assert_eq!(lines.next_line().as_deref(), Some("{\"b\":2}"));
        lines.push(b"tail");
        assert_eq!(lines.finish().as_deref(), Some("tail"));
        assert_eq!(lines.finish(), None);
    }

    #[test]
    fn resume_state_round_trips_once() {
        let temp = tempfile::tempdir().unwrap();
        let path = write_state(temp.path(), "driver", &vec!["a".to_string()]).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let state: Vec<String> = read_state(&path).unwrap();
        assert_eq!(state, ["a"]);
        assert!(!path.exists(), "the next image consumes its state file");
    }

    #[test]
    fn an_unreadable_resume_format_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.json");
        fs::write(&path, br#"{"format":999,"state":[]}"#).unwrap();
        let error = read_state::<Vec<String>>(&path).unwrap_err();
        assert!(format!("{error:#}").contains("999"), "{error:#}");
    }

    #[test]
    fn exec_keeps_the_pid_arguments_environment_and_named_descriptors() {
        // The child re-executes a shell script that reports what it inherited.
        let temp = tempfile::tempdir().unwrap();
        let report = temp.path().join("report");
        let script = temp.path().join("next");
        write_script(
            &script,
            &format!(
                "echo \"$$ $1 ${{{DRIVER_RESUME_ENV}}} $ST3_TEST_KEEP\" > {}; \
                 cat <&7 >> {}",
                report.display(),
                report.display()
            ),
        );
        let state = temp.path().join("state.json");
        fs::write(&state, b"{}").unwrap();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "reexec::tests::exec_child",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("ST3_REEXEC_TEST_SCRIPT", &script)
            .env("ST3_REEXEC_TEST_STATE", &state)
            .env("ST3_TEST_KEEP", "kept")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = command.spawn().unwrap();
        let pid = child.id();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success());
        let report = fs::read_to_string(&report).unwrap();
        let mut lines = report.lines();
        assert_eq!(
            lines.next().unwrap(),
            format!("{pid} first {} kept", state.display())
        );
        assert_eq!(lines.next().unwrap(), "through the kept descriptor");
    }

    #[test]
    fn a_recorded_stop_abandons_the_exec_and_restores_the_signal_mask() {
        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("next");
        write_script(&script, "exit 0");
        let state = temp.path().join("state.json");
        fs::write(&state, b"{}").unwrap();
        let (read, _write) = std::os::unix::net::UnixStream::pair().unwrap();
        use std::os::unix::io::AsRawFd as _;
        let fd = read.as_raw_fd();
        let error = exec_unless_stopped(&script, DRIVER_RESUME_ENV, &state, &[fd], &|| true);
        assert_eq!(error.kind(), io::ErrorKind::Interrupted, "{error}");
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        assert_ne!(flags & libc::FD_CLOEXEC, 0, "close-on-exec is restored");
        let mut mask = unsafe { std::mem::zeroed::<libc::sigset_t>() };
        unsafe {
            libc::pthread_sigmask(libc::SIG_BLOCK, std::ptr::null(), &mut mask);
        }
        for signal in STOP_SIGNALS {
            assert_eq!(unsafe { libc::sigismember(&mask, signal) }, 0);
        }
    }

    /// Runs only inside [`exec_keeps_the_pid_arguments_environment_and_named_descriptors`].
    #[test]
    fn exec_child() {
        let (Some(script), Some(state)) = (
            std::env::var_os("ST3_REEXEC_TEST_SCRIPT"),
            std::env::var_os("ST3_REEXEC_TEST_STATE"),
        ) else {
            return;
        };
        let (read, mut write) = std::os::unix::net::UnixStream::pair().unwrap();
        write.write_all(b"through the kept descriptor\n").unwrap();
        drop(write);
        use std::os::unix::io::AsRawFd as _;
        let fd = read.as_raw_fd();
        assert_eq!(unsafe { libc::dup2(fd, 7) }, 7);
        unsafe {
            libc::fcntl(7, libc::F_SETFD, libc::FD_CLOEXEC);
        }
        let error = exec_with_arguments(
            Path::new(&script),
            &[OsString::from("first")],
            DRIVER_RESUME_ENV,
            Path::new(&state),
            &[7],
        );
        panic!("exec failed: {error}");
    }
}
