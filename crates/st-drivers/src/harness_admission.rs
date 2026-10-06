//! Measured admission of an exact installed producer, without release allowlists.
//!
//! Evidence belongs to executable contents, runtime/installation identity, shipped adapter bytes,
//! and the probe implementation. Version strings alone never authorize native delivery. Probes
//! use empty disposable homes and a local model fixture; they never open a managed conversation.

pub(crate) mod fixture;
mod omp;

use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::driver_diagnostic::{Driver, Publisher, Reason, Source, Stage};

pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_secs(45);
const SCHEMA: &str = "st.harness-admission.v1";
const MAX_CAPTURE: u64 = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Check {
    ExtensionLoad,
    ApiContract,
    Lifecycle,
    IdleEdge,
    ApprovalCorrelation,
    NativeConsumption,
}

impl Check {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ExtensionLoad => "extensionLoad",
            Self::ApiContract => "apiContract",
            Self::Lifecycle => "lifecycle",
            Self::IdleEdge => "idleEdge",
            Self::ApprovalCorrelation => "approvalCorrelation",
            Self::NativeConsumption => "nativeConsumption",
        }
    }

    fn reason(self) -> Reason {
        match self {
            Self::ExtensionLoad => Reason::AdmissionExtensionLoad,
            Self::ApiContract => Reason::AdmissionApiContract,
            Self::Lifecycle => Reason::AdmissionLifecycle,
            Self::IdleEdge => Reason::AdmissionIdleEdge,
            Self::ApprovalCorrelation => Reason::AdmissionApprovalCorrelation,
            Self::NativeConsumption => Reason::AdmissionNativeConsumption,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Measurement {
    pub check: Check,
    pub passed: bool,
}

pub(crate) fn measurement(check: Check, passed: bool) -> Measurement {
    Measurement { check, passed }
}

fn checks(driver: Driver) -> &'static [Check] {
    match driver {
        Driver::Omp => &[
            Check::ExtensionLoad,
            Check::Lifecycle,
            Check::IdleEdge,
            Check::ApprovalCorrelation,
            Check::NativeConsumption,
        ],
        Driver::OpenCode => &[
            Check::ApiContract,
            Check::Lifecycle,
            Check::IdleEdge,
            Check::ApprovalCorrelation,
            Check::NativeConsumption,
        ],
        _ => &[],
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Identity {
    driver: Driver,
    version: String,
    executable: PathBuf,
    executable_digest: String,
    adapter_digest: String,
    probe_digest: String,
}

// A person's exception belongs to the installed build and adapter, independently of the
// measurement implementation. It is never stored as passing measurement evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct InstalledIdentity {
    driver: Driver,
    executable: PathBuf,
    executable_digest: String,
    adapter_digest: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct OverrideRecord {
    schema: String,
    identity: InstalledIdentity,
    reason: String,
    recorded_at: u64,
}

const OVERRIDE_SCHEMA: &str = "st.harness-admission-override.v1";

fn installed_identity(
    binary: &str,
    driver: Driver,
    adapter: Option<&Path>,
) -> Result<InstalledIdentity> {
    anyhow::ensure!(
        !checks(driver).is_empty(),
        "driver has no measured admission contract"
    );
    anyhow::ensure!(
        driver != Driver::Omp || adapter.is_some(),
        "omp needs its shipped extension"
    );
    let executable = resolve_executable(binary)?;
    Ok(InstalledIdentity {
        executable_digest: executable_identity(&executable)?,
        executable,
        driver,
        adapter_digest: digest(&adapter.map(fs::read).transpose()?.unwrap_or_default()),
    })
}

fn override_path(cache: &Path, identity: &InstalledIdentity) -> Result<PathBuf> {
    Ok(cache.join(format!(
        "{}-{}.override.json",
        identity.driver.as_str(),
        digest(&serde_json::to_vec(identity)?)
    )))
}

/// Explicit local operator exception. Does not run the producer, access credentials, or
/// manufacture probe results. The CLI restricts this action to a person's terminal.
pub fn override_build(
    binary: &str,
    driver: Driver,
    adapter: Option<&Path>,
    cache: &Path,
    reason: &str,
) -> Result<PathBuf> {
    let reason = reason.trim();
    anyhow::ensure!(
        !reason.is_empty() && reason.len() <= 1024,
        "an override needs a reason of 1 to 1024 bytes"
    );
    let identity = installed_identity(binary, driver, adapter)?;
    fs::create_dir_all(cache)?;
    let path = override_path(cache, &identity)?;
    let _lock = CacheLock::acquire(&path.with_extension("lock"))?;
    // Check again before persisting, just as a measurement does after running its probe.
    anyhow::ensure!(
        identity == installed_identity(binary, driver, adapter)?,
        "producer or shipped adapter changed during override"
    );
    let record = OverrideRecord {
        schema: OVERRIDE_SCHEMA.into(),
        identity,
        reason: reason.into(),
        recorded_at: SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64,
    };
    crate::fsatomic::replace(
        &path,
        &serde_json::to_vec_pretty(&record)?,
        crate::fsatomic::Staging::new(".harness-override"),
        crate::fsatomic::Durability::FsyncFileAndDir,
    )?;
    // The exception is user-local, like the admission cache, and contains an operator's reason.
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    Ok(path)
}

/// Revoke this build's exception. Previous failed measurements stay intact and apply again.
pub fn revoke_override(
    binary: &str,
    driver: Driver,
    adapter: Option<&Path>,
    cache: &Path,
) -> Result<PathBuf> {
    let path = override_path(cache, &installed_identity(binary, driver, adapter)?)?;
    fs::create_dir_all(cache)?;
    let _lock = CacheLock::acquire(&path.with_extension("lock"))?;
    match fs::remove_file(&path) {
        Ok(()) => File::open(cache)?.sync_all()?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(path)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct ProbeDiagnostic {
    phase: String,
    outcome: String,
    exit_code: Option<i32>,
    elapsed_ms: u64,
    stderr_tail: String,
    detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Record {
    schema: String,
    identity: Identity,
    observed_at: u64,
    measurements: Vec<Measurement>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    probe_failure: Option<Reason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    diagnostic: Option<ProbeDiagnostic>,
}

/// The gate result used by both products, including the bounded diagnostic boundary.
#[derive(Debug, Clone)]
pub struct Admission {
    pub version: Option<String>,
    pub failed: Option<Reason>,
    pub cache_path: Option<PathBuf>,
    pub override_reason: Option<String>,
}

impl Admission {
    pub fn passed(&self) -> bool {
        self.failed.is_none()
    }

    pub fn explanation(&self, driver: Driver) -> String {
        if let Some(reason) = &self.override_reason {
            return format!(
                "{} admission allowed by a person's exact-build override: {reason}; checks were bypassed, not measured as passing",
                driver.as_str()
            );
        }
        match self.failed {
            None => format!("{} admission passed", driver.as_str()),
            Some(reason) => format!(
                "{} {} admission failed at {}; native delivery is unavailable. Repair the producer/adapter contract and remove {} to repeat the isolated checks. A person may explicitly allow this installed build with `st admission override {} --binary <installed-executable> --reason <reason>`, then restart the affected seat",
                driver.as_str(),
                self.version.as_deref().unwrap_or("unknown version"),
                reason.as_str(),
                self.cache_path
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "the failed admission cache record (if present)".into()),
                driver.as_str(),
            ),
        }
    }

    pub fn publish(&self, publisher: &mut Publisher) {
        if let Some(reason) = self.failed {
            publisher.publish(
                Stage::VersionGate,
                reason,
                if reason == Reason::VersionProbeFailed {
                    Source::VersionProbe
                } else {
                    Source::AdmissionProbe
                },
            );
        } else {
            publisher.clear(Stage::VersionGate);
        }
    }

    pub fn support(&self) -> crate::driver_diagnostic::Support {
        use crate::driver_diagnostic::Support;
        if self.override_reason.is_some() {
            Support::Unknown
        } else if self.passed() {
            Support::Supported
        } else {
            Support::Unsupported
        }
    }
}

/// Admit before taking ownership of the live seat. The cache is shared by this user's seats;
/// callers retain their different policies: omp refuses launch, OpenCode disables delivery.
pub fn admit(binary: &str, driver: Driver, adapter: Option<&Path>) -> Admission {
    let cache = crate::run::harness_state_root().join("harness-admission");
    match admit_at(binary, driver, adapter, &cache) {
        Ok(result) => result,
        Err(error) => {
            tracing::warn!("{} admission indeterminate: {error:#}", driver.as_str());
            Admission {
                version: None,
                failed: Some(Reason::AdmissionIndeterminate),
                cache_path: None,
                override_reason: None,
            }
        }
    }
}

fn admit_at(
    binary: &str,
    driver: Driver,
    adapter: Option<&Path>,
    cache: &Path,
) -> Result<Admission> {
    let installed = installed_identity(binary, driver, adapter)?;
    let exception_path = override_path(cache, &installed)?;
    // Read before even --version: a scratch/runtime incompatibility can be the reason for the
    // exception. Replacements cannot inherit it, and corrupt exceptions never authorize launch.
    if let Some(record) = fs::read(&exception_path)
        .ok()
        .filter(|raw| raw.len() <= 16_384)
        .and_then(|raw| serde_json::from_slice::<OverrideRecord>(&raw).ok())
        .filter(|record| {
            record.schema == OVERRIDE_SCHEMA
                && record.identity == installed
                && !record.reason.trim().is_empty()
                && record.reason.len() <= 1024
        })
    {
        tracing::warn!(
            "{} admission allowed by a person's exact-build override: {}; checks bypassed",
            driver.as_str(),
            record.reason
        );
        return Ok(Admission {
            version: None,
            failed: None,
            cache_path: Some(exception_path),
            override_reason: Some(record.reason),
        });
    }
    let executable = installed.executable;
    let scratch = Scratch::new()?;
    let mut command = scratch.command(&executable);
    command.arg("--version");
    let mut child = scratch.spawn(&mut command, "version")?;
    let status = child.wait_until(Instant::now() + Duration::from_secs(5))?;
    drop(child);
    let printed = scratch.capture("version.stdout")?;
    let version = status
        .success()
        .then(|| crate::harness_version::find_release(&printed, driver.as_str()))
        .flatten()
        .map(|(token, _)| token.to_owned());
    let Some(version) = version else {
        return Ok(Admission {
            version: None,
            failed: Some(Reason::VersionProbeFailed),
            cache_path: None,
            override_reason: None,
        });
    };
    let identity = Identity {
        driver,
        version: version.clone(),
        executable: executable.clone(),
        executable_digest: installed.executable_digest,
        adapter_digest: installed.adapter_digest,
        probe_digest: probe_digest(),
    };
    fs::create_dir_all(cache)?;
    let key = digest(&serde_json::to_vec(&identity)?);
    let path = cache.join(format!("{}-{key}.json", driver.as_str()));
    let _lock = CacheLock::acquire(&path.with_extension("lock"))?;
    let cached = fs::read(&path)
        .ok()
        .filter(|raw| raw.len() <= 16_384)
        .and_then(|raw| serde_json::from_slice::<Record>(&raw).ok())
        .filter(|record| {
            record.schema == SCHEMA
                && record.identity == identity
                && valid_measurements(driver, &record.measurements)
                && matches!(
                    record.probe_failure,
                    None | Some(Reason::AdmissionIndeterminate | Reason::AdmissionLaunchRefused)
                )
        });
    let record = match cached {
        Some(record) => record,
        None => {
            let started = Instant::now();
            let probe = match driver {
                Driver::Omp => omp::probe(
                    &executable,
                    adapter.context("omp needs its shipped extension")?,
                    &scratch,
                ),
                Driver::OpenCode => crate::opencode_session::probe_admission(&executable, &scratch)
                    .map(|measurements| {
                        let diagnostic = measurements.iter().find(|m| !m.passed).map(|m| {
                            ProbeDiagnostic {
                                phase: m.check.as_str().into(),
                                outcome: "missingEvidence".into(),
                                exit_code: None,
                                elapsed_ms: started.elapsed().as_millis() as u64,
                                stderr_tail: scratch.stderr_tail("opencode.stderr").unwrap_or_default(),
                                detail: format!("required {} evidence was not observed", m.check.as_str()),
                            }
                        });
                        (measurements, diagnostic)
                    }),
                _ => unreachable!(),
            };
            let (measurements, probe_failure, diagnostic) = match probe {
                Ok((measurements, diagnostic)) => {
                    let failure = diagnostic.as_ref()
                        .filter(|diagnostic| diagnostic.outcome == "launch-refused")
                        .map(|_| Reason::AdmissionLaunchRefused);
                    (measurements, failure, diagnostic)
                }
                Err(error) => {
                    tracing::warn!("{} admission indeterminate: {error:#}", driver.as_str());
                    (
                        checks(driver)
                            .iter()
                            .map(|&check| measurement(check, false))
                            .collect(),
                        Some(Reason::AdmissionIndeterminate),
                        Some(ProbeDiagnostic {
                            phase: "probe".into(),
                            outcome: "error".into(),
                            exit_code: None,
                            elapsed_ms: started.elapsed().as_millis() as u64,
                            stderr_tail: scratch.stderr_tail(&format!("{}.stderr", driver.as_str()))
                                .unwrap_or_default(),
                            detail: format!("{error:#}"),
                        }),
                    )
                }
            };
            if let Some(diagnostic) = &diagnostic {
                tracing::warn!(?diagnostic, "{} admission probe failed", driver.as_str());
            }
            // Replacing the binary or adapter while the probe ran cannot produce a pass for
            // either build. Next launch measures the replacement under its own identity.
            anyhow::ensure!(
                identity.executable_digest == executable_identity(&executable)?
                    && identity.adapter_digest
                        == digest(&adapter.map(fs::read).transpose()?.unwrap_or_default()),
                "producer or shipped adapter changed during admission"
            );
            anyhow::ensure!(
                valid_measurements(driver, &measurements),
                "incomplete admission evidence"
            );
            let record = Record {
                schema: SCHEMA.into(),
                identity,
                observed_at: SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64,
                measurements,
                probe_failure,
                diagnostic,
            };
            crate::fsatomic::replace(
                &path,
                &serde_json::to_vec_pretty(&record)?,
                crate::fsatomic::Staging::new(".harness-admission"),
                crate::fsatomic::Durability::FsyncFileAndDir,
            )?;
            record
        }
    };
    if let Some(diagnostic) = &record.diagnostic {
        tracing::warn!(?diagnostic, "{} cached admission probe failed", driver.as_str());
    }
    Ok(Admission {
        version: Some(version),
        failed: record.probe_failure.or_else(|| {
            record
                .measurements
                .iter()
                .find(|measurement| !measurement.passed)
                .map(|measurement| measurement.check.reason())
        }),
        cache_path: Some(path),
        override_reason: None,
    })
}

fn valid_measurements(driver: Driver, measurements: &[Measurement]) -> bool {
    measurements.len() == checks(driver).len()
        && measurements
            .iter()
            .zip(checks(driver))
            .all(|(measured, required)| measured.check == *required)
}

fn probe_digest() -> String {
    digest(
        concat!(
            include_str!("harness_admission.rs"),
            include_str!("harness_admission/omp.rs"),
            include_str!("harness_admission/omp-probe.ts"),
            include_str!("harness_admission/fixture.rs"),
            include_str!("opencode_session/admission.rs"),
            include_str!("opencode_session.rs"),
            include_str!("harness_version.rs")
        )
        .as_bytes(),
    )
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub(crate) fn resolve_executable(binary: &str) -> Result<PathBuf> {
    let path = Path::new(binary);
    let resolved = if path.components().count() > 1 || path.is_absolute() {
        path.to_owned()
    } else {
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|root| root.join(path))
            .find(|candidate| {
                candidate.is_file()
                    && candidate
                        .metadata()
                        .is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
            })
            .with_context(|| format!("{binary} is not executable on PATH"))?
    };
    fs::canonicalize(resolved).with_context(|| format!("resolving installed executable {binary}"))
}

/// Fingerprint the executable, script runtime and installation metadata without traversing
/// package assets. Bundled code belongs to the executable; package manifests and lockfiles
/// identify dependency releases. This is build identity, not an integrity scan of every asset.
fn executable_identity(path: &Path) -> Result<String> {
    let mut hash = Sha256::new();
    // Changing the identity policy invalidates both old measurements and old exceptions.
    hash.update(b"st.installed-producer.executable-runtime-metadata.v2");
    hash_identity_file(path, &mut hash)?;
    let mut prefix = [0_u8; 256];
    let n = File::open(path)?.read(&mut prefix)?;
    if let Some(shebang) = String::from_utf8_lossy(&prefix[..n])
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("#!"))
    {
        let parts = shebang.split_whitespace().collect::<Vec<_>>();
        if let Some(interpreter) = parts.first() {
            let interpreter = if interpreter.ends_with("/env") {
                parts.get(1).context("missing script interpreter")?
            } else {
                interpreter
            };
            hash_identity_file(&resolve_executable(interpreter)?, &mut hash)?;
        }
    }
    if let Some(package) = path
        .ancestors()
        .skip(1)
        .take(3)
        .find(|p| p.join("package.json").is_file())
    {
        hash_identity_file(&package.join("package.json"), &mut hash)?;
        for name in [
            "package-lock.json",
            "npm-shrinkwrap.json",
            "bun.lock",
            "bun.lockb",
            "yarn.lock",
            "pnpm-lock.yaml",
        ] {
            hash_optional_identity_file(&package.join(name), &mut hash)?;
        }
        // npm's hidden lock lives beside the installed packages, including scoped packages.
        if let Some(modules) = package
            .ancestors()
            .find(|p| p.file_name().is_some_and(|n| n == "node_modules"))
        {
            hash_optional_identity_file(&modules.join(".package-lock.json"), &mut hash)?;
        }
    }
    Ok(format!("{hash:x}", hash = hash.finalize()))
}

fn hash_optional_identity_file(path: &Path, hash: &mut Sha256) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => hash_identity_file(path, hash),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            hash.update(b"absent");
            hash.update((path.as_os_str().as_encoded_bytes().len() as u64).to_le_bytes());
            hash.update(path.as_os_str().as_encoded_bytes());
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

fn hash_identity_file(path: &Path, hash: &mut Sha256) -> Result<()> {
    let resolved = fs::canonicalize(path)?;
    let mut file = File::open(&resolved)?;
    let metadata = file.metadata()?;
    anyhow::ensure!(
        metadata.is_file(),
        "producer identity is not a regular file"
    );
    hash.update(b"file");
    for identity_path in [path, resolved.as_path()] {
        let bytes = identity_path.as_os_str().as_encoded_bytes();
        hash.update((bytes.len() as u64).to_le_bytes());
        hash.update(bytes);
    }
    hash.update(metadata.permissions().mode().to_le_bytes());
    hash.update(metadata.len().to_le_bytes());
    // Streaming keeps memory bounded; there is no installation byte/file-count limit.
    let mut buffer = [0_u8; 65536];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    Ok(())
}

struct CacheLock(File);
impl CacheLock {
    fn acquire(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?;
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                return Ok(Self(file));
            }
            let error = std::io::Error::last_os_error();
            anyhow::ensure!(
                error.kind() == std::io::ErrorKind::WouldBlock,
                "admission cache lock: {error}"
            );
            anyhow::ensure!(
                Instant::now() < deadline,
                "timed out waiting for another admission probe"
            );
            thread::sleep(Duration::from_millis(25));
        }
    }
}
impl Drop for CacheLock {
    fn drop(&mut self) {
        let _ = unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

pub(crate) struct Scratch {
    root: tempfile::TempDir,
}
impl Scratch {
    pub(crate) fn new() -> Result<Self> {
        let root = tempfile::Builder::new()
            .prefix("st-harness-admission-")
            .tempdir()?;
        for dir in [
            "home",
            "config",
            "data",
            "state",
            "cache",
            "runtime",
            "tmp",
            "sessions",
            "pty",
            "agent",
            "workspace/.git",
        ] {
            fs::create_dir_all(root.path().join(dir))?;
        }
        Ok(Self { root })
    }
    pub(crate) fn path(&self, name: &str) -> PathBuf {
        self.root.path().join(name)
    }
    pub(crate) fn command(&self, binary: &Path) -> Command {
        let mut command = Command::new(binary);
        command
            .env_clear()
            .current_dir(self.path("workspace"))
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("LANG", "C.UTF-8")
            .env("TERM", "dumb")
            .env("HOME", self.path("home"))
            .env("TMPDIR", self.path("tmp"))
            .env("XDG_CONFIG_HOME", self.path("config"))
            .env("XDG_DATA_HOME", self.path("data"))
            .env("XDG_STATE_HOME", self.path("state"))
            .env("XDG_CACHE_HOME", self.path("cache"))
            .env("XDG_RUNTIME_DIR", self.path("runtime"))
            .env("PTY_ROOT", self.path("pty"))
            .env("PI_CODING_AGENT_DIR", self.path("agent"))
            .env("PI_OFFLINE", "1")
            .env("PI_SKIP_VERSION_CHECK", "1")
            .env("OMP_SKIP_SETUP", "1");
        command
    }
    pub(crate) fn spawn(&self, command: &mut Command, name: &str) -> Result<ProbeProcess> {
        use std::os::unix::process::CommandExt;
        command
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(File::create(self.path(&format!("{name}.stdout")))?)
            .stderr(File::create(self.path(&format!("{name}.stderr")))?);
        Ok(ProbeProcess(command.spawn()?))
    }
    fn stderr_tail(&self, name: &str) -> Result<String> {
        use std::io::{Seek, SeekFrom};
        let path = self.path(name);
        if !path.exists() {
            return Ok(String::new());
        }
        let mut file = File::open(path)?;
        let length = file.metadata()?.len();
        file.seek(SeekFrom::Start(length.saturating_sub(4096)))?;
        let mut bytes = Vec::new();
        file.take(4096).read_to_end(&mut bytes)?;
        Ok(String::from_utf8_lossy(&bytes).chars()
            .filter(|character| !character.is_control() || matches!(character, '\n' | '\t'))
            .collect())
    }
    pub(crate) fn capture(&self, name: &str) -> Result<String> {
        let path = self.path(name);
        if !path.exists() {
            return Ok(String::new());
        }
        let file = File::open(path)?;
        anyhow::ensure!(
            file.metadata()?.len() <= MAX_CAPTURE,
            "admission capture exceeded byte bound"
        );
        let mut text = String::new();
        file.take(MAX_CAPTURE + 1).read_to_string(&mut text)?;
        anyhow::ensure!(
            text.len() as u64 <= MAX_CAPTURE,
            "admission capture exceeded byte bound"
        );
        Ok(text)
    }
}

pub(crate) struct ProbeProcess(pub(crate) Child);
impl ProbeProcess {
    fn wait_until(&mut self, deadline: Instant) -> Result<std::process::ExitStatus> {
        loop {
            if let Some(exit) = self.0.try_wait()? {
                return Ok(exit);
            }
            anyhow::ensure!(Instant::now() < deadline, "admission process timed out");
            thread::sleep(Duration::from_millis(25));
        }
    }
}
impl Drop for ProbeProcess {
    fn drop(&mut self) {
        // The disposable provider may have started its channel/tools. Kill the entire probe
        // group even if the direct child already exited, and always reap the direct child.
        unsafe {
            libc::kill(-(self.0.id() as i32), libc::SIGKILL);
        }
        let _ = self.0.wait();
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::Value;

    pub(crate) fn measured_omp() -> Value {
        serde_json::from_str(include_str!(
            "../tests/fixtures/harness-admission/omp-18.1.22.json"
        ))
        .unwrap()
    }

    pub(crate) fn fake_omp(directory: &Path) -> PathBuf {
        fake_omp_with_version(directory, "99.42.7")
    }

    fn fake_omp_with_version(directory: &Path, version: &str) -> PathBuf {
        let python = resolve_executable("python3").unwrap();
        let path = directory.join("omp");
        fs::write(
            directory.join("capture.json"),
            serde_json::to_vec(&measured_omp()).unwrap(),
        )
        .unwrap();
        let source = directory.join("omp.source");
        fs::write(
            &source,
            format!(
                "#!{}\n{}",
                python.display(),
                include_str!("../tests/fixtures/harness-admission/fake-omp.py")
                    .replace("omp/99.42.7", &format!("omp/{version}"))
            ),
        )
        .unwrap();
        assert!(
            Command::new("install")
                .args(["-m", "755"])
                .arg(source)
                .arg(&path)
                .status()
                .unwrap()
                .success()
        );
        path
    }

    fn admit_fake(directory: &Path) -> Admission {
        let extension = directory.join("extension.ts");
        if !extension.exists() {
            fs::write(&extension, "fixture adapter").unwrap();
        }
        admit_at(
            directory.join("omp").to_str().unwrap(),
            Driver::Omp,
            Some(&extension),
            &directory.join("cache"),
        )
        .unwrap()
    }

    fn fake_opencode(directory: &Path) -> PathBuf {
        let python = resolve_executable("python3").unwrap();
        let source = directory.join("opencode.source");
        let binary = directory.join("opencode");
        fs::write(
            &source,
            format!(
                "#!{}\n{}",
                python.display(),
                include_str!("../tests/fixtures/harness-admission/fake-opencode.py")
            ),
        )
        .unwrap();
        assert!(
            Command::new("install")
                .args(["-m", "755"])
                .arg(source)
                .arg(&binary)
                .status()
                .unwrap()
                .success()
        );
        binary
    }

    #[test]
    fn unseen_opencode_release_passes_and_reuses_cache() {
        let directory = tempfile::tempdir().unwrap();
        let binary = fake_opencode(directory.path());
        for _ in 0..2 {
            let result = admit_at(
                binary.to_str().unwrap(),
                Driver::OpenCode,
                None,
                &directory.path().join("cache"),
            )
            .unwrap();
            assert!(result.passed(), "{result:?}");
            assert_eq!(result.version.as_deref(), Some("99.42.7"));
        }
        assert_eq!(
            fs::read_to_string(directory.path().join("launches")).unwrap(),
            "probe\n"
        );
    }

    #[test]
    fn unseen_opencode_release_fails_each_named_check() {
        for &check in checks(Driver::OpenCode) {
            let directory = tempfile::tempdir().unwrap();
            let binary = fake_opencode(directory.path());
            fs::write(directory.path().join("failure"), check.as_str()).unwrap();
            let result = admit_at(
                binary.to_str().unwrap(),
                Driver::OpenCode,
                None,
                &directory.path().join("cache"),
            )
            .unwrap();
            assert_eq!(result.failed, Some(check.reason()), "{check:?}: {result:?}");
        }
    }

    #[test]
    fn unseen_exact_release_passes_and_reuses_measured_cache() {
        let directory = tempfile::tempdir().unwrap();
        fake_omp(directory.path());
        let first = admit_fake(directory.path());
        assert!(first.passed(), "{first:?}");
        assert_eq!(first.version.as_deref(), Some("99.42.7"));
        let again = admit_fake(directory.path());
        assert!(again.passed());
        assert_eq!(first.cache_path, again.cache_path);
        assert_eq!(
            fs::read_to_string(directory.path().join("launches")).unwrap(),
            "probe\n"
        );
    }

    /// #1339 admitted the 18.6 minor on main. That historical capture must not bypass
    /// this installed build's measurements, including a new patch in the same minor.
    #[test]
    fn omp_18_6_requires_all_measurements_instead_of_minor_admission() {
        for version in ["18.6.0", "18.6.99"] {
            for failure in
                std::iter::once(None).chain(checks(Driver::Omp).iter().copied().map(Some))
            {
                let directory = tempfile::tempdir().unwrap();
                fake_omp_with_version(directory.path(), version);
                if let Some(check) = failure {
                    let mut capture = measured_omp();
                    fail_omp(&mut capture, check);
                    fs::write(
                        directory.path().join("capture.json"),
                        serde_json::to_vec(&capture).unwrap(),
                    )
                    .unwrap();
                }
                let first = admit_fake(directory.path());
                assert_eq!(first.version.as_deref(), Some(version));
                assert_eq!(
                    first.failed,
                    failure.map(Check::reason),
                    "{version}: {first:?}"
                );
                assert_eq!(first.passed(), failure.is_none());
                let again = admit_fake(directory.path());
                assert_eq!(again.failed, first.failed);
                assert_eq!(again.cache_path, first.cache_path);
                assert_eq!(
                    fs::read_to_string(directory.path().join("launches")).unwrap(),
                    "probe\n"
                );
            }
        }
    }

    #[test]
    fn operator_exception_preserves_refusal_and_revocation_restores_it_for_both_harnesses() {
        for driver in [Driver::Omp, Driver::OpenCode] {
            let directory = tempfile::tempdir().unwrap();
            let binary = if driver == Driver::Omp {
                let binary = fake_omp(directory.path());
                let mut capture = measured_omp();
                fail_omp(&mut capture, Check::NativeConsumption);
                fs::write(
                    directory.path().join("capture.json"),
                    serde_json::to_vec(&capture).unwrap(),
                )
                .unwrap();
                binary
            } else {
                fs::write(directory.path().join("failure"), "nativeConsumption").unwrap();
                fake_opencode(directory.path())
            };
            let adapter = directory.path().join("extension.ts");
            fs::write(&adapter, "fixture adapter").unwrap();
            let adapter = (driver == Driver::Omp).then_some(adapter.as_path());
            let cache = directory.path().join("cache");
            let binary = binary.to_str().unwrap();
            let refused = admit_at(binary, driver, adapter, &cache).unwrap();
            assert_eq!(refused.failed, Some(Reason::AdmissionNativeConsumption));
            let measured_path = refused.cache_path.unwrap();
            let measured_bytes = fs::read(&measured_path).unwrap();
            let path = override_build(
                binary,
                driver,
                adapter,
                &cache,
                "local runtime cannot run the probe",
            )
            .unwrap();
            let overridden = admit_at(binary, driver, adapter, &cache).unwrap();
            assert!(overridden.passed());
            assert_eq!(
                overridden.override_reason.as_deref(),
                Some("local runtime cannot run the probe")
            );
            assert_eq!(
                overridden.support(),
                crate::driver_diagnostic::Support::Unknown
            );
            assert!(
                overridden
                    .explanation(driver)
                    .contains("checks were bypassed")
            );
            assert_eq!(overridden.cache_path, Some(path.clone()));
            let exception: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            assert_eq!(exception["schema"], OVERRIDE_SCHEMA);
            assert!(exception.get("measurements").is_none());
            assert_eq!(fs::read(&measured_path).unwrap(), measured_bytes);
            assert_eq!(
                fs::read_to_string(directory.path().join("launches")).unwrap(),
                "probe\n"
            );
            fs::write(&path, "broken exception").unwrap();
            assert_eq!(
                admit_at(binary, driver, adapter, &cache).unwrap().failed,
                Some(Reason::AdmissionNativeConsumption)
            );
            override_build(binary, driver, adapter, &cache, "explicit exception").unwrap();
            revoke_override(binary, driver, adapter, &cache).unwrap();
            assert!(!path.exists());
            assert_eq!(
                admit_at(binary, driver, adapter, &cache).unwrap().failed,
                Some(Reason::AdmissionNativeConsumption)
            );
            assert_eq!(fs::read(&measured_path).unwrap(), measured_bytes);
        }
    }

    #[test]
    fn operator_exception_bypasses_even_the_version_child_but_never_a_changed_build_or_adapter() {
        let directory = tempfile::tempdir().unwrap();
        let binary = directory.path().join("omp");
        let sh = resolve_executable("sh").unwrap();
        fs::write(
            &binary,
            format!(
                "#!{}\nprintf called >> '{}'\nexit 1\n",
                sh.display(),
                directory.path().join("called").display()
            ),
        )
        .unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
        let adapter = directory.path().join("extension.ts");
        fs::write(&adapter, "first adapter").unwrap();
        let cache = directory.path().join("cache");
        let binary = binary.to_str().unwrap();
        assert!(override_build(binary, Driver::Omp, Some(&adapter), &cache, " ").is_err());
        let first = override_build(
            binary,
            Driver::Omp,
            Some(&adapter),
            &cache,
            "probe cannot run here",
        )
        .unwrap();
        assert!(
            admit_at(binary, Driver::Omp, Some(&adapter), &cache)
                .unwrap()
                .passed()
        );
        assert!(
            !directory.path().join("called").exists(),
            "neither the operator command nor overridden launch spawns any probe child"
        );
        fs::write(&adapter, "replacement adapter").unwrap();
        assert_eq!(
            admit_at(binary, Driver::Omp, Some(&adapter), &cache)
                .unwrap()
                .failed,
            Some(Reason::VersionProbeFailed)
        );
        fs::write(&adapter, "first adapter").unwrap();
        let mut bytes = fs::read(binary).unwrap();
        bytes.extend_from_slice(b"# replaced same-version build\n");
        fs::write(binary, bytes).unwrap();
        assert_eq!(
            admit_at(binary, Driver::Omp, Some(&adapter), &cache)
                .unwrap()
                .failed,
            Some(Reason::VersionProbeFailed)
        );
        assert!(
            first.exists(),
            "the old build's explicit exception remains auditable"
        );
    }

    pub(crate) fn fail_omp(fixture: &mut Value, check: Check) {
        match check {
            Check::ExtensionLoad => {
                fixture["events"][0]["sendUserMessage"] = Value::String("undefined".into())
            }
            Check::Lifecycle => fixture["events"]
                .as_array_mut()
                .unwrap()
                .retain(|e| e["type"] != "turn_start"),
            Check::IdleEdge => fixture["events"]
                .as_array_mut()
                .unwrap()
                .retain(|e| e["type"] != "idle_sample"),
            Check::ApprovalCorrelation => {
                for event in fixture["events"].as_array_mut().unwrap() {
                    if event["type"] == "tool_approval_resolved" {
                        event["event"]["toolCallId"] = Value::String("different-call".into());
                    }
                }
            }
            Check::NativeConsumption => {
                for event in fixture["events"].as_array_mut().unwrap() {
                    if event["type"] == "message_end"
                        && event["event"]["message"]["stopReason"] == "stop"
                    {
                        event["event"]["message"]["content"] = serde_json::json!([]);
                    }
                }
            }
            Check::ApiContract => unreachable!(),
        }
    }

    #[test]
    fn unseen_release_fails_each_named_check_and_caches_refusal() {
        for &check in checks(Driver::Omp) {
            let directory = tempfile::tempdir().unwrap();
            fake_omp(directory.path());
            let mut fixture = measured_omp();
            fail_omp(&mut fixture, check);
            fs::write(
                directory.path().join("capture.json"),
                serde_json::to_vec(&fixture).unwrap(),
            )
            .unwrap();
            let result = admit_fake(directory.path());
            assert_eq!(result.failed, Some(check.reason()), "{check:?}: {result:?}");
            assert_eq!(admit_fake(directory.path()).failed, result.failed);
            assert_eq!(
                fs::read_to_string(directory.path().join("launches")).unwrap(),
                "probe\n"
            );
        }
    }

    #[test]
    fn replaced_same_version_build_and_adapter_cannot_reuse_a_pass() {
        let directory = tempfile::tempdir().unwrap();
        let binary = fake_omp(directory.path());
        let first = admit_fake(directory.path());
        assert!(first.passed());
        let replacement = directory.path().join("replacement");
        fs::write(
            &replacement,
            format!(
                "{}\n# changed build\n",
                fs::read_to_string(&binary).unwrap()
            ),
        )
        .unwrap();
        assert!(
            Command::new("install")
                .args(["-m", "755"])
                .arg(replacement)
                .arg(&binary)
                .status()
                .unwrap()
                .success()
        );
        let second = admit_fake(directory.path());
        assert!(second.passed());
        assert_ne!(first.cache_path, second.cache_path);
        fs::write(directory.path().join("extension.ts"), "changed adapter").unwrap();
        let third = admit_fake(directory.path());
        assert!(third.passed());
        assert_ne!(second.cache_path, third.cache_path);
        assert_eq!(
            fs::read_to_string(directory.path().join("launches")).unwrap(),
            "probe\nprobe\nprobe\n"
        );
    }

    #[test]
    fn incomplete_or_malformed_cache_is_remeasured() {
        let directory = tempfile::tempdir().unwrap();
        fake_omp(directory.path());
        let first = admit_fake(directory.path());
        let cache = first.cache_path.unwrap();
        let mut record: Value = serde_json::from_slice(&fs::read(&cache).unwrap()).unwrap();
        record["measurements"].as_array_mut().unwrap().pop();
        fs::write(&cache, serde_json::to_vec(&record).unwrap()).unwrap();
        assert!(admit_fake(directory.path()).passed());
        fs::write(&cache, "broken json").unwrap();
        assert!(admit_fake(directory.path()).passed());
        assert_eq!(
            fs::read_to_string(directory.path().join("launches")).unwrap(),
            "probe\nprobe\nprobe\n"
        );
    }

    #[test]
    fn concurrent_starts_share_one_probe() {
        let directory = tempfile::tempdir().unwrap();
        fake_omp(directory.path());
        fs::write(directory.path().join("extension.ts"), "fixture adapter").unwrap();
        thread::scope(|scope| {
            let tasks = (0..4)
                .map(|_| scope.spawn(|| admit_fake(directory.path())))
                .collect::<Vec<_>>();
            for task in tasks {
                assert!(task.join().unwrap().passed());
            }
        });
        assert_eq!(
            fs::read_to_string(directory.path().join("launches")).unwrap(),
            "probe\n"
        );
    }

    #[test]
    fn malformed_probe_cannot_inherit_other_passing_evidence() {
        let directory = tempfile::tempdir().unwrap();
        let binary = fake_omp(directory.path());
        fs::write(directory.path().join("malformed"), "").unwrap();
        let scratch = Scratch::new().unwrap();
        let extension = directory.path().join("extension.ts");
        fs::write(&extension, "fixture").unwrap();
        assert!(omp::probe(&binary, &extension, &scratch).is_err());
        let admission = admit_fake(directory.path());
        assert_eq!(admission.failed, Some(Reason::AdmissionIndeterminate));
        assert_eq!(admit_fake(directory.path()).failed, admission.failed);
        assert_eq!(
            fs::read_to_string(directory.path().join("launches")).unwrap(),
            "probe\nprobe\n"
        );
    }

    #[test]
    fn large_installation_assets_and_cycles_do_not_block_identity_or_override() {
        use std::os::unix::fs::symlink;
        let directory = tempfile::tempdir().unwrap();
        let binary = fake_omp(directory.path());
        fs::write(directory.path().join("package.json"), "{}").unwrap();
        let asset = File::create(directory.path().join("model.bin")).unwrap();
        asset.set_len(2 * 1024 * 1024 * 1024).unwrap();
        symlink(directory.path(), directory.path().join("cycle")).unwrap();
        let first = executable_identity(&binary).unwrap();
        fs::write(directory.path().join("unrelated-asset"), "data").unwrap();
        assert_eq!(first, executable_identity(&binary).unwrap());
        let state = directory.path().join("state");
        let adapter = directory.path().join("extension.ts");
        fs::write(&adapter, "fixture").unwrap();
        override_build(
            binary.to_str().unwrap(),
            Driver::Omp,
            Some(&adapter),
            &state,
            "offline fixture",
        )
        .unwrap();
        revoke_override(
            binary.to_str().unwrap(),
            Driver::Omp,
            Some(&adapter),
            &state,
        )
        .unwrap();
    }

    #[test]
    fn package_manifest_and_lockfile_changes_invalidate_identity() {
        let directory = tempfile::tempdir().unwrap();
        let binary = fake_omp(directory.path());
        let manifest = directory.path().join("package.json");
        fs::write(
            &manifest,
            r#"{"version":"1","dependencies":{"fixture":"1"}}"#,
        )
        .unwrap();
        let first = executable_identity(&binary).unwrap();
        fs::write(
            &manifest,
            r#"{"version":"1","dependencies":{"fixture":"2"}}"#,
        )
        .unwrap();
        let second = executable_identity(&binary).unwrap();
        assert_ne!(first, second);
        let lock = directory.path().join("package-lock.json");
        fs::write(&lock, "one").unwrap();
        let third = executable_identity(&binary).unwrap();
        assert_ne!(second, third);
        fs::write(&lock, "two").unwrap();
        assert_ne!(third, executable_identity(&binary).unwrap());
        fs::remove_file(&lock).unwrap();
        assert_eq!(second, executable_identity(&binary).unwrap());
    }

    #[test]
    fn npm_hidden_lock_and_runtime_replacement_change_identity() {
        let directory = tempfile::tempdir().unwrap();
        let modules = directory.path().join("node_modules");
        let package = modules.join("@fixture/producer");
        fs::create_dir_all(package.join("dist")).unwrap();
        fs::write(package.join("package.json"), "{}").unwrap();
        let runtime = directory.path().join("runtime");
        fs::write(&runtime, "runtime one").unwrap();
        let binary = package.join("dist/cli.js");
        fs::write(&binary, format!("#!{}\nfixture", runtime.display())).unwrap();
        let first = executable_identity(&binary).unwrap();
        let lock = modules.join(".package-lock.json");
        fs::write(&lock, "dependencies one").unwrap();
        let second = executable_identity(&binary).unwrap();
        assert_ne!(first, second);
        fs::write(&lock, "dependencies two").unwrap();
        let third = executable_identity(&binary).unwrap();
        assert_ne!(second, third);
        fs::write(&runtime, "runtime two").unwrap();
        assert_ne!(third, executable_identity(&binary).unwrap());
    }

    #[test]
    fn timed_out_probe_reaps_its_entire_process_group() {
        let scratch = Scratch::new().unwrap();
        let sh = resolve_executable("sh").unwrap();
        let mut command = scratch.command(&sh);
        command.args(["-c", "sleep 60 & wait"]);
        let mut child = scratch.spawn(&mut command, "timeout").unwrap();
        let pid = child.0.id() as i32;
        assert!(
            child
                .wait_until(Instant::now() + Duration::from_millis(75))
                .is_err()
        );
        drop(child);
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    }

    #[test]
    fn guarded_launcher_is_admitted_without_real_profile_or_seat() {
        let directory = tempfile::tempdir().unwrap();
        let native = fake_omp(directory.path());
        let guarded = directory.path().join("guarded-omp");
        let sh = resolve_executable("sh").unwrap();
        fs::write(&guarded, format!(
            "#!{}\nif [ \"$1\" = --version ]; then exec {} \"$@\"; fi\ncount=0\nfor arg in \"$@\"; do [ \"$arg\" != --session-dir ] || count=$((count + 1)); done\nif [ \"$count\" != 1 ]; then echo 'missing session directory' >&2; exit 64; fi\nif [ -z \"${{ST_AGENT:-}}\" ]; then echo 'missing actor' >&2; exit 70; fi\nunset PI_CODING_AGENT_DIR\nexec {} \"$@\"\n",
            sh.display(), native.display(), native.display(),
        )).unwrap();
        fs::set_permissions(&guarded, fs::Permissions::from_mode(0o700)).unwrap();
        let adapter = directory.path().join("extension.ts");
        fs::write(&adapter, "fixture").unwrap();
        let admission = admit_at(
            guarded.to_str().unwrap(), Driver::Omp, Some(&adapter), &directory.path().join("cache"),
        ).unwrap();
        assert!(admission.passed(), "{admission:?}");
    }

    #[test]
    fn early_exit_records_stderr_status_and_failed_phase_in_cache() {
        let directory = tempfile::tempdir().unwrap();
        let binary = directory.path().join("omp");
        let sh = resolve_executable("sh").unwrap();
        fs::write(&binary, format!(
            "#!{}\nif [ \"$1\" = --version ]; then echo omp/18.6.0; exit; fi\necho 'launcher: session contract rejected' >&2\nexit 64\n",
            sh.display(),
        )).unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
        let admission = admit_fake(directory.path());
        assert_eq!(admission.failed, Some(Reason::AdmissionLaunchRefused));
        let record: Record = serde_json::from_slice(
            &fs::read(admission.cache_path.unwrap()).unwrap(),
        ).unwrap();
        let diagnostic = record.diagnostic.unwrap();
        assert_eq!(diagnostic.outcome, "launch-refused");
        assert_eq!(diagnostic.exit_code, Some(64));
        assert_eq!(diagnostic.phase, "launch");
        assert_eq!(diagnostic.stderr_tail, "launcher: session contract rejected\n");
    }

    #[test]
    #[ignore = "requires an installed OpenCode; isolated loopback model, no credentials"]
    fn installed_opencode_admission() {
        let binary = resolve_executable("opencode").unwrap();
        let scratch = Scratch::new().unwrap();
        let result = crate::opencode_session::probe_admission(&binary, &scratch);
        for capture in [
            "opencode.stdout",
            "opencode.stderr",
            "api.txt",
            "events.jsonl",
            "messages.json",
        ] {
            eprintln!("{capture}: {}", scratch.capture(capture).unwrap());
        }
        let result = result.unwrap();
        assert!(result.iter().all(|m| m.passed), "{result:?}");
    }

    #[test]
    #[ignore = "requires an installed omp; isolated loopback model, no credentials"]
    fn installed_omp_admission() {
        let binary = resolve_executable("omp").unwrap();
        let scratch = Scratch::new().unwrap();
        let extension = scratch.path("omp-channel.ts");
        fs::write(&extension, include_str!("../hooks/omp-channel.ts")).unwrap();
        let result = omp::probe(&binary, &extension, &scratch);
        for capture in ["omp.stdout", "omp.stderr", "events.jsonl", "channel.jsonl"] {
            eprintln!("{capture}: {}", scratch.capture(capture).unwrap());
        }
        let (result, diagnostic) = result.unwrap();
        assert!(diagnostic.is_none(), "{diagnostic:?}");
        assert!(result.iter().all(|m| m.passed), "{result:?}");
    }
}
