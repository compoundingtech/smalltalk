use std::collections::BTreeMap;
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use chrono::{SecondsFormat, Utc};
use parking_lot::Mutex;
use st2_resource_wasip2::{
    CapabilityContext, CapabilityModule, CapabilityPhase, InterruptionReason,
    InvocationControl as ExecutorInvocationControl, InvocationStore,
};
use wasmtime::component::{HasSelf, Linker};

mod bindings {
    wasmtime::component::bindgen!({
        path: "../../wit/vista",
        world: "vista-provider",
    });
}

use bindings::compoundingtech::st2_vista::vista::{
    ArtifactRequest, ArtifactResponse, CommandFailure, ExitStatus, Host, SourceObservation,
    SourceSnapshot, VistaError,
};

const IMPORT_NAME: &str = "compoundingtech:st2-vista/vista@0.1.0";
const MAX_STDOUT_BYTES: usize = 1024 * 1024;
const MAX_STDERR_BYTES: usize = 64 * 1024;
const MAX_VERSION: u64 = 9_999_999_999_999_999_999;
const SNAPSHOT_DIGEST_BYTES: usize = 32;
const MAX_CACHED_SNAPSHOTS: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VistaConfig {
    pub executable: PathBuf,
    pub cwd: PathBuf,
    pub deadline: Duration,
}

impl VistaConfig {
    pub fn resolve(
        executable: impl AsRef<Path>,
        cwd: impl Into<PathBuf>,
        deadline: Duration,
    ) -> Result<Self, &'static str> {
        if deadline.is_zero() || deadline > Duration::from_secs(60) {
            return Err("Vista deadline is invalid");
        }
        let executable =
            resolve_executable(executable.as_ref()).ok_or("Vista executable is unavailable")?;
        let cwd = cwd.into();
        if !cwd.is_absolute() {
            return Err("Vista cwd must be absolute");
        }
        Ok(Self {
            executable,
            cwd,
            deadline,
        })
    }
}

#[derive(Clone)]
pub struct VistaModule {
    config: VistaConfig,
    cache: Arc<Mutex<SnapshotCache>>,
}

impl VistaModule {
    pub fn new(config: VistaConfig) -> Self {
        Self {
            config,
            cache: Arc::new(Mutex::new(SnapshotCache::default())),
        }
    }
}

#[derive(Debug, Clone)]
struct CachedSource {
    manifest_json: Vec<u8>,
    observed_at: String,
}

#[derive(Default)]
struct SnapshotCache {
    sources: BTreeMap<[u8; SNAPSHOT_DIGEST_BYTES], CachedSource>,
}

impl SnapshotCache {
    fn get(&self, digest: &[u8; SNAPSHOT_DIGEST_BYTES]) -> Option<&CachedSource> {
        self.sources.get(digest)
    }

    fn insert(&mut self, digest: [u8; SNAPSHOT_DIGEST_BYTES], source: CachedSource) {
        if !self.sources.contains_key(&digest)
            && self.sources.len() >= MAX_CACHED_SNAPSHOTS
            && let Some(evicted) = self.sources.keys().next().copied()
        {
            self.sources.remove(&evicted);
        }
        self.sources.insert(digest, source);
    }
}

pub struct VistaInvocation {
    config: VistaConfig,
    cache: Arc<Mutex<SnapshotCache>>,
    prior_digest: Option<[u8; SNAPSHOT_DIGEST_BYTES]>,
    current_source: Option<CachedSource>,
    control: Arc<ProcessControl>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum Termination {
    None = 0,
    Cancelled = 1,
    TimedOut = 2,
}

struct ProcessControl {
    termination: AtomicU8,
    child: Mutex<ChildOwnership>,
    #[cfg(not(test))]
    invocation: ExecutorInvocationControl,
    #[cfg(test)]
    invocation: Option<ExecutorInvocationControl>,
}

enum ChildOwnership {
    Pending,
    Live(i32),
    Reaping,
    Reaped,
}

impl ProcessControl {
    fn new(invocation: ExecutorInvocationControl) -> Self {
        Self {
            termination: AtomicU8::new(Termination::None as u8),
            child: Mutex::new(ChildOwnership::Pending),
            #[cfg(not(test))]
            invocation,
            #[cfg(test)]
            invocation: Some(invocation),
        }
    }

    #[cfg(test)]
    fn detached() -> Self {
        Self {
            termination: AtomicU8::new(Termination::None as u8),
            child: Mutex::new(ChildOwnership::Pending),
            invocation: None,
        }
    }

    fn termination(&self) -> Termination {
        match self.termination.load(Ordering::Acquire) {
            1 => Termination::Cancelled,
            2 => Termination::TimedOut,
            _ => match self.executor_interruption() {
                Some(InterruptionReason::Cancelled) => Termination::Cancelled,
                Some(InterruptionReason::TimedOut) => Termination::TimedOut,
                None => Termination::None,
            },
        }
    }

    fn executor_interruption(&self) -> Option<InterruptionReason> {
        #[cfg(not(test))]
        {
            self.invocation.interruption_reason()
        }
        #[cfg(test)]
        {
            self.invocation
                .as_ref()
                .and_then(ExecutorInvocationControl::interruption_reason)
        }
    }

    fn install(&self, process_group: i32) {
        let mut ownership = self.child.lock();
        debug_assert!(matches!(*ownership, ChildOwnership::Pending));
        *ownership = ChildOwnership::Live(process_group);
        if self.termination() != Termination::None {
            let _ = kill_process_group(process_group);
        }
    }

    fn terminate(&self, reason: Termination) -> bool {
        let changed = self
            .termination
            .compare_exchange(
                Termination::None as u8,
                reason as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok();
        if changed && let ChildOwnership::Live(process_group) = *self.child.lock() {
            let _ = kill_process_group(process_group);
        }
        changed
    }

    fn synchronize_interruption(&self) -> Termination {
        let reason = self.termination();
        if reason != Termination::None {
            self.terminate(reason);
        }
        reason
    }

    fn kill_live_group(&self) {
        if let ChildOwnership::Live(process_group) = *self.child.lock() {
            let _ = kill_process_group(process_group);
        }
    }

    fn reap_exited(
        &self,
        child: &mut std::process::Child,
    ) -> std::io::Result<std::process::ExitStatus> {
        let mut ownership = self.child.lock();
        debug_assert!(matches!(*ownership, ChildOwnership::Live(_)));
        *ownership = ChildOwnership::Reaping;
        let status = child.wait();
        *ownership = ChildOwnership::Reaped;
        status
    }

    fn kill_and_reap(&self, child: &mut std::process::Child) {
        let mut ownership = self.child.lock();
        if let ChildOwnership::Live(process_group) = *ownership {
            let _ = kill_process_group(process_group);
        }
        *ownership = ChildOwnership::Reaping;
        let _ = child.wait();
        *ownership = ChildOwnership::Reaped;
    }
}

impl CapabilityModule for VistaModule {
    type Invocation = VistaInvocation;

    fn import_names(&self) -> &'static [&'static str] {
        &[IMPORT_NAME]
    }

    fn add_to_linker(
        &self,
        linker: &mut Linker<InvocationStore<Self::Invocation>>,
    ) -> Result<(), wasmtime::Error> {
        bindings::VistaProvider::add_to_linker::<_, HasSelf<_>>(linker, |state| state)
    }

    fn begin(&self, context: CapabilityContext<'_>) -> Self::Invocation {
        let prior_digest = match context.phase() {
            CapabilityPhase::Describe => None,
            CapabilityPhase::Observe(request) => request
                .prior_digest
                .as_ref()
                .map(|digest| *digest.as_bytes()),
        };
        VistaInvocation {
            config: self.config.clone(),
            cache: Arc::clone(&self.cache),
            prior_digest,
            current_source: None,
            control: Arc::new(ProcessControl::new(context.control().clone())),
        }
    }
}

impl Host for InvocationStore<VistaInvocation> {
    fn get(&mut self, request: ArtifactRequest) -> Result<ArtifactResponse, VistaError> {
        self.capability_mut().get(request)
    }

    fn bind_snapshot(&mut self, digest: Vec<u8>) -> Result<(), VistaError> {
        self.capability_mut().bind_snapshot(digest)
    }
}

impl VistaInvocation {
    fn get(&mut self, request: ArtifactRequest) -> Result<ArtifactResponse, VistaError> {
        if !request_is_valid(&request) {
            return Err(VistaError::Denied);
        }
        let prior = self
            .prior_digest
            .as_ref()
            .and_then(|digest| self.cache.lock().get(digest).cloned());
        match self.control.termination() {
            Termination::Cancelled => return Err(VistaError::Cancelled),
            Termination::TimedOut => return Err(VistaError::DeadlineExceeded),
            Termination::None => {}
        }

        let mut command = Command::new(&self.config.executable);
        command
            .arg("artifact")
            .arg("get")
            .arg(&request.slug)
            .arg(format!("v{}", request.version))
            .arg("--output")
            .arg("json")
            .current_dir(&self.config.cwd)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // SAFETY: this runs in the freshly-forked child before exec and calls only async-signal-safe
        // setpgid. The dedicated process group is the cancellation/reaping boundary.
        unsafe {
            command.pre_exec(|| {
                if libc::setpgid(0, 0) == -1 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
        let mut child = command.spawn().map_err(|_| VistaError::Unavailable)?;
        self.control.install(child.id() as i32);
        let Some(stdout) = child.stdout.take() else {
            self.control.kill_and_reap(&mut child);
            return Err(VistaError::Unavailable);
        };
        let Some(stderr) = child.stderr.take() else {
            self.control.kill_and_reap(&mut child);
            return Err(VistaError::Unavailable);
        };
        let stdout_reader = match thread::Builder::new()
            .name("st2-vista-stdout".into())
            .spawn(move || drain_bounded(stdout, MAX_STDOUT_BYTES))
        {
            Ok(reader) => reader,
            Err(_) => {
                self.control.kill_and_reap(&mut child);
                return Err(VistaError::Unavailable);
            }
        };
        let stderr_reader = match thread::Builder::new()
            .name("st2-vista-stderr".into())
            .spawn(move || drain_bounded(stderr, MAX_STDERR_BYTES))
        {
            Ok(reader) => reader,
            Err(_) => {
                self.control.kill_and_reap(&mut child);
                let _ = stdout_reader.join();
                return Err(VistaError::Unavailable);
            }
        };
        let (completed_tx, completed_rx) = mpsc::sync_channel(1);
        let deadline = Instant::now() + self.config.deadline;
        let deadline_control = Arc::clone(&self.control);
        let timer = match thread::Builder::new()
            .name("st2-vista-deadline".into())
            .spawn(move || {
                loop {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        deadline_control.terminate(Termination::TimedOut);
                        return;
                    }
                    match completed_rx.recv_timeout(remaining.min(Duration::from_millis(10))) {
                        Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => return,
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                    }
                    if deadline_control.synchronize_interruption() != Termination::None {
                        return;
                    }
                }
            }) {
            Ok(timer) => timer,
            Err(_) => {
                self.control.kill_and_reap(&mut child);
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(VistaError::Unavailable);
            }
        };
        if wait_without_reaping(child.id()).is_err() {
            self.control.kill_and_reap(&mut child);
            let _ = completed_tx.send(());
            let _ = timer.join();
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            return Err(VistaError::Unavailable);
        }
        let stdout = stdout_reader.join();
        let stderr = stderr_reader.join();
        self.control.kill_live_group();
        let _ = completed_tx.send(());
        let _ = timer.join();
        let status = self.control.reap_exited(&mut child);
        let status = status.map_err(|_| VistaError::Unavailable)?;
        let (stdout, stdout_truncated) = stdout.map_err(|_| VistaError::Unavailable)??;
        let (stderr, stderr_truncated) = stderr.map_err(|_| VistaError::Unavailable)??;
        match self.control.termination() {
            Termination::Cancelled => return Err(VistaError::Cancelled),
            Termination::TimedOut => return Err(VistaError::DeadlineExceeded),
            Termination::None => {}
        }
        if stdout_truncated || stderr_truncated {
            return Err(VistaError::ResourceExhausted);
        }
        let exit = status.code().map_or_else(
            || ExitStatus::Signal(status.signal().unwrap_or(0)),
            ExitStatus::Code,
        );
        match exit {
            ExitStatus::Code(0) => {
                let observed_at = prior
                    .as_ref()
                    .filter(|prior| prior.manifest_json == stdout)
                    .map_or_else(
                        || Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
                        |prior| prior.observed_at.clone(),
                    );
                let source = CachedSource {
                    manifest_json: stdout,
                    observed_at,
                };
                let current = source_to_wit(&source);
                self.current_source = Some(source);
                Ok(ArtifactResponse::Ok(SourceObservation {
                    current,
                    previous: prior.as_ref().map(source_to_wit),
                }))
            }
            exit @ (ExitStatus::Code(_) | ExitStatus::Signal(_)) => {
                Ok(ArtifactResponse::CommandFailed(CommandFailure {
                    stderr,
                    exit,
                }))
            }
        }
    }

    fn bind_snapshot(&mut self, digest: Vec<u8>) -> Result<(), VistaError> {
        let digest: [u8; SNAPSHOT_DIGEST_BYTES] =
            digest.try_into().map_err(|_| VistaError::Denied)?;
        let source = self.current_source.take().ok_or(VistaError::Unavailable)?;
        self.cache.lock().insert(digest, source);
        Ok(())
    }
}

fn source_to_wit(source: &CachedSource) -> SourceSnapshot {
    SourceSnapshot {
        manifest_json: source.manifest_json.clone(),
        observed_at: source.observed_at.clone(),
    }
}

fn request_is_valid(request: &ArtifactRequest) -> bool {
    valid_slug(&request.slug) && (1..=MAX_VERSION).contains(&request.version)
}

fn valid_slug(slug: &str) -> bool {
    !slug.is_empty()
        && slug.len() <= 128
        && slug.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || (byte == b'-' && index > 0)
        })
        && !slug.ends_with('-')
        && !slug.contains("--")
}

fn drain_bounded(
    mut input: impl std::io::Read,
    limit: usize,
) -> Result<(Vec<u8>, bool), VistaError> {
    let mut retained = Vec::with_capacity(limit.min(64 * 1024));
    let mut truncated = false;
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let read = input
            .read(&mut buffer)
            .map_err(|_| VistaError::Unavailable)?;
        if read == 0 {
            break;
        }
        let remaining = limit.saturating_sub(retained.len());
        retained
            .write_all(&buffer[..read.min(remaining)])
            .map_err(|_| VistaError::Unavailable)?;
        truncated |= read > remaining;
    }
    Ok((retained, truncated))
}

fn kill_process_group(process_group: i32) -> bool {
    // SAFETY: negative pid addresses the process group created by pre_exec; SIGKILL makes the
    // deadline a hard bound even when the provider subprocess ignores graceful signals.
    unsafe { libc::kill(-process_group, libc::SIGKILL) == 0 }
}

fn wait_without_reaping(pid: u32) -> std::io::Result<()> {
    loop {
        // SAFETY: `info` is initialized for the kernel, and WNOWAIT keeps the child waitable so its
        // process-group identity cannot be recycled before ownership is fenced.
        let result = unsafe {
            let mut info = std::mem::zeroed::<libc::siginfo_t>();
            libc::waitid(libc::P_PID, pid, &mut info, libc::WEXITED | libc::WNOWAIT)
        };
        if result == 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

fn resolve_executable(executable: &Path) -> Option<PathBuf> {
    if executable.is_absolute() {
        return executable_is_runnable(executable).then(|| executable.to_path_buf());
    }
    let validation_cwd = std::env::current_dir().ok()?;
    let search_path = std::env::var_os("PATH");
    resolve_executable_at(executable, &validation_cwd, search_path.as_deref())
}

fn resolve_executable_at(
    executable: &Path,
    validation_cwd: &Path,
    search_path: Option<&std::ffi::OsStr>,
) -> Option<PathBuf> {
    debug_assert!(validation_cwd.is_absolute());
    if executable.components().count() > 1 {
        let candidate = validation_cwd.join(executable);
        return executable_is_runnable(&candidate).then_some(candidate);
    }
    let search_path = search_path?;
    std::env::split_paths(search_path)
        .map(|directory| {
            let directory = if directory.is_absolute() {
                directory
            } else {
                validation_cwd.join(directory)
            };
            directory.join(executable)
        })
        .find(|candidate| executable_is_runnable(candidate))
}

fn executable_is_runnable(path: &Path) -> bool {
    std::fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(test)]
mod tests {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt as _;

    use super::*;

    fn write_executable(path: &Path, contents: &str) {
        std::fs::write(path, contents).unwrap();
        let mut permissions = std::fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(path, permissions).unwrap();
    }

    fn invocation(
        executable: &Path,
        cwd: &Path,
        deadline: Duration,
        control: Arc<ProcessControl>,
    ) -> VistaInvocation {
        VistaInvocation {
            config: VistaConfig::resolve(executable, cwd.to_path_buf(), deadline).unwrap(),
            cache: Arc::new(Mutex::new(SnapshotCache::default())),
            prior_digest: None,
            current_source: None,
            control,
        }
    }

    fn request(slug: &str, version: u64) -> ArtifactRequest {
        ArtifactRequest {
            slug: slug.into(),
            version,
        }
    }

    #[test]
    fn fixed_command_uses_exact_read_only_argv() {
        let temporary = tempfile::tempdir().unwrap();
        let executable = temporary.path().join("vista");
        write_executable(
            &executable,
            "#!/bin/sh\nprintf '%s\\n' \"$#|$1|$2|$3|$4|$5|$6\"\n",
        );
        let mut invocation = invocation(
            &executable,
            temporary.path(),
            Duration::from_secs(1),
            Arc::new(ProcessControl::detached()),
        );
        let response = invocation.get(request("release-notes", 7)).unwrap();
        let ArtifactResponse::Ok(observation) = response else {
            panic!("the successful command did not return a source observation");
        };
        assert_eq!(
            observation.current.manifest_json,
            b"6|artifact|get|release-notes|v7|--output|json\n"
        );
    }

    #[test]
    fn dynamic_artifact_identity_is_validated_before_spawn() {
        let temporary = tempfile::tempdir().unwrap();
        let executable = temporary.path().join("vista");
        let marker = temporary.path().join("spawned");
        write_executable(&executable, "#!/bin/sh\n: > \"$PWD/spawned\"\n");
        let mut first = invocation(
            &executable,
            temporary.path(),
            Duration::from_secs(1),
            Arc::new(ProcessControl::detached()),
        );
        for denied in [
            request("", 7),
            request("-leading", 7),
            request("trailing-", 7),
            request("two--dashes", 7),
            request("Upper", 7),
            request("release-notes", 0),
            request("release-notes", MAX_VERSION + 1),
        ] {
            assert!(matches!(first.get(denied), Err(VistaError::Denied)));
        }
        assert!(!marker.exists());
        assert!(matches!(
            first.get(request("release-notes", 7)),
            Ok(ArtifactResponse::Ok(_))
        ));
        let mut second = invocation(
            &executable,
            temporary.path(),
            Duration::from_secs(1),
            Arc::new(ProcessControl::detached()),
        );
        assert!(matches!(
            second.get(request("other-valid-slug", MAX_VERSION)),
            Ok(ArtifactResponse::Ok(_))
        ));
        assert!(marker.exists());
    }

    #[test]
    fn artifact_version_is_bounded_by_the_canonical_nineteen_digit_contract() {
        let temporary = tempfile::tempdir().unwrap();
        let executable = temporary.path().join("vista");
        write_executable(&executable, "#!/bin/sh\nexit 0\n");
        assert!(request_is_valid(&request("release", MAX_VERSION)));
        assert!(!request_is_valid(&request("release", MAX_VERSION + 1)));
        assert!(
            VistaConfig::resolve(
                &executable,
                temporary.path().to_path_buf(),
                Duration::from_secs(1),
            )
            .is_ok()
        );
    }

    #[test]
    fn snapshot_binding_recovers_the_previous_source_by_digest() {
        let temporary = tempfile::tempdir().unwrap();
        let executable = temporary.path().join("vista");
        write_executable(&executable, "#!/bin/sh\nprintf '{\"schemaVersion\":1}'\n");
        let cache = Arc::new(Mutex::new(SnapshotCache::default()));
        let mut first = invocation(
            &executable,
            temporary.path(),
            Duration::from_secs(1),
            Arc::new(ProcessControl::detached()),
        );
        first.cache = Arc::clone(&cache);
        let ArtifactResponse::Ok(first_observation) = first.get(request("release", 7)).unwrap()
        else {
            panic!("the successful command did not return a source observation");
        };
        assert!(first_observation.previous.is_none());
        let digest = [7; SNAPSHOT_DIGEST_BYTES];
        first.bind_snapshot(digest.to_vec()).unwrap();

        let mut second = invocation(
            &executable,
            temporary.path(),
            Duration::from_secs(1),
            Arc::new(ProcessControl::detached()),
        );
        second.cache = cache;
        second.prior_digest = Some(digest);
        let ArtifactResponse::Ok(second_observation) = second.get(request("release", 7)).unwrap()
        else {
            panic!("the successful command did not return a source observation");
        };
        let previous = second_observation
            .previous
            .expect("the bound source should be recovered");
        assert_eq!(
            previous.manifest_json,
            second_observation.current.manifest_json
        );
        assert_eq!(previous.observed_at, second_observation.current.observed_at);
    }

    #[test]
    fn cancelled_before_spawn_is_effect_free() {
        let temporary = tempfile::tempdir().unwrap();
        let executable = temporary.path().join("vista");
        let marker = temporary.path().join("spawned");
        write_executable(&executable, "#!/bin/sh\ntouch \"$PWD/spawned\"\n");
        let control = Arc::new(ProcessControl::detached());
        control.terminate(Termination::Cancelled);
        let mut invocation = invocation(
            &executable,
            temporary.path(),
            Duration::from_secs(1),
            control,
        );
        assert!(matches!(
            invocation.get(request("valid", 1)),
            Err(VistaError::Cancelled)
        ));
        assert!(!marker.exists());
    }

    #[test]
    fn deadline_owns_the_process_group_until_inherited_output_pipes_close() {
        let temporary = tempfile::tempdir().unwrap();
        let executable = temporary.path().join("vista");
        let holder = temporary.path().join("pipe-holder");
        std::os::unix::fs::symlink(std::env::current_exe().unwrap(), &holder).unwrap();
        let fifo = temporary.path().join("block");
        let started = temporary.path().join("started");
        for path in [&fifo, &started] {
            let path = CString::new(path.as_os_str().as_bytes()).unwrap();
            // SAFETY: the pathname is a live NUL-terminated byte string owned for the call.
            assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        }
        write_executable(
            &executable,
            "#!/bin/sh\n(trap '' HUP; exec \"$PWD/pipe-holder\" --ignored --exact vista::tests::inherited_pipe_holder --nocapture) &\nread marker < \"$PWD/started\"\nprintf 'direct child exited\\n'\nexit 0\n",
        );
        let control = Arc::new(ProcessControl::detached());
        let mut invocation = invocation(
            &executable,
            temporary.path(),
            Duration::from_millis(100),
            Arc::clone(&control),
        );
        assert!(matches!(
            invocation.get(request("valid", 1)),
            Err(VistaError::DeadlineExceeded)
        ));
        assert!(matches!(*control.child.lock(), ChildOwnership::Reaped));
    }

    #[test]
    fn cancellation_owns_the_process_group_until_inherited_output_pipes_close() {
        let temporary = tempfile::tempdir().unwrap();
        let executable = temporary.path().join("vista");
        let holder = temporary.path().join("pipe-holder");
        std::os::unix::fs::symlink(std::env::current_exe().unwrap(), &holder).unwrap();
        for name in ["block", "started", "cancel-ready"] {
            let path = temporary.path().join(name);
            let path = CString::new(path.as_os_str().as_bytes()).unwrap();
            // SAFETY: the pathname is a live NUL-terminated byte string owned for the call.
            assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        }
        write_executable(
            &executable,
            "#!/bin/sh\n(trap '' HUP; exec \"$PWD/pipe-holder\" --ignored --exact vista::tests::inherited_pipe_holder --nocapture) &\nread marker < \"$PWD/started\"\nprintf 'ready\\n' > \"$PWD/cancel-ready\"\nprintf 'direct child exited\\n'\nexit 0\n",
        );
        let control = Arc::new(ProcessControl::detached());
        let mut invocation = invocation(
            &executable,
            temporary.path(),
            Duration::from_secs(10),
            Arc::clone(&control),
        );
        let worker = thread::spawn(move || invocation.get(request("valid", 1)));
        let mut ready = String::new();
        let mut ready_pipe = std::fs::File::open(temporary.path().join("cancel-ready")).unwrap();
        std::io::Read::read_to_string(&mut ready_pipe, &mut ready).unwrap();
        assert_eq!(ready, "ready\n");
        assert!(control.terminate(Termination::Cancelled));
        assert!(matches!(worker.join().unwrap(), Err(VistaError::Cancelled)));
        assert!(matches!(*control.child.lock(), ChildOwnership::Reaped));
    }

    #[test]
    fn successful_completion_kills_descendants_that_closed_output_pipes() {
        let temporary = tempfile::tempdir().unwrap();
        let executable = temporary.path().join("vista");
        let holder = temporary.path().join("pipe-holder");
        std::os::unix::fs::symlink(std::env::current_exe().unwrap(), &holder).unwrap();
        for name in ["block", "started"] {
            let path = temporary.path().join(name);
            let path = CString::new(path.as_os_str().as_bytes()).unwrap();
            // SAFETY: the pathname is a live NUL-terminated byte string owned for the call.
            assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        }
        std::fs::File::create(temporary.path().join("descendant-lock")).unwrap();
        write_executable(
            &executable,
            "#!/bin/sh\n(trap '' HUP; exec \"$PWD/pipe-holder\" --ignored --exact vista::tests::closed_pipe_descendant_holder --nocapture) &\nread marker < \"$PWD/started\"\nexit 0\n",
        );
        let control = Arc::new(ProcessControl::detached());
        let mut invocation = invocation(
            &executable,
            temporary.path(),
            Duration::from_secs(1),
            Arc::clone(&control),
        );
        assert!(matches!(
            invocation.get(request("valid", 1)),
            Ok(ArtifactResponse::Ok(_))
        ));
        let lock_path = temporary.path().join("descendant-lock");
        let (acquired_tx, acquired_rx) = mpsc::sync_channel(0);
        let waiter = thread::spawn(move || {
            let lock = std::fs::OpenOptions::new()
                .write(true)
                .open(lock_path)
                .unwrap();
            // SAFETY: `lock` owns a live file descriptor for the duration of the call.
            let result =
                unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&lock), libc::LOCK_EX) };
            let _ = acquired_tx.send(result);
        });
        let acquired = acquired_rx.recv_timeout(Duration::from_secs(1));
        if let Err(error) = &acquired {
            drop(acquired_rx);
            let pid: i32 = std::fs::read_to_string(temporary.path().join("descendant-pid"))
                .unwrap()
                .parse()
                .unwrap();
            // SAFETY: the fixture wrote its live pid after acquiring the lock.
            unsafe { libc::kill(pid, libc::SIGKILL) };
            waiter.join().unwrap();
            panic!("the descendant retained its lock after provider completion: {error}");
        }
        assert_eq!(acquired.unwrap(), 0);
        waiter.join().unwrap();
        assert!(matches!(*control.child.lock(), ChildOwnership::Reaped));
    }

    #[test]
    #[ignore = "subprocess fixture for inherited output pipe ownership"]
    fn inherited_pipe_holder() {
        let mut started = std::fs::OpenOptions::new()
            .write(true)
            .open("started")
            .unwrap();
        writeln!(started, "ready").unwrap();
        drop(started);
        let _blocked = std::fs::File::open("block").unwrap();
    }

    #[test]
    #[ignore = "subprocess fixture for descendant process-group ownership"]
    fn closed_pipe_descendant_holder() {
        let lock = std::fs::OpenOptions::new()
            .write(true)
            .open("descendant-lock")
            .unwrap();
        // SAFETY: `lock` owns a live file descriptor for the duration of the call.
        assert_eq!(
            unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&lock), libc::LOCK_EX) },
            0
        );
        std::fs::write("descendant-pid", std::process::id().to_string()).unwrap();
        // SAFETY: the subprocess fixture intentionally releases inherited output pipes while
        // retaining the process-group-owned lock.
        unsafe {
            libc::close(libc::STDOUT_FILENO);
            libc::close(libc::STDERR_FILENO);
        }
        let mut started = std::fs::OpenOptions::new()
            .write(true)
            .open("started")
            .unwrap();
        writeln!(started, "ready").unwrap();
        drop(started);
        let _blocked = std::fs::File::open("block").unwrap();
    }
}
