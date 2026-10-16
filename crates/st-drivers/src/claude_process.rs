//! Local Claude process ownership. Environment/session labels locate a witness; they never
//! replace the kernel's parent and birth-time proof. This is isolation from inherited context,
//! not a security boundary against a same-user process rewriting all of st's local state.
use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::Path,
    time::{Duration, Instant},
};

const FILE: &str = "claude-primary-process.json";
const LOCK: &str = "claude-primary-process.lock";
/// Private launch transport of the owning daemon's install setting, not an auth credential.
/// The daemon always overrides a seat declaration's value; only a fresh claim samples it.
pub const ENFORCEMENT_ENV: &str = "ST3_CLAUDE_PRIMARY_OWNERSHIP";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Process {
    pub pid: u32,
    pub parent: u32,
    pub birth: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Owner {
    version: u8,
    session: String,
    sequence: u64,
    runtime: String,
    wrapper: Process,
    provider: Option<Process>,
    closed: bool,
}

#[derive(Clone, Debug)]
pub enum Caller<'a> {
    Channel,
    Wrapper,
    Hook { trampoline: &'a str },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Admission {
    Primary,
    Legacy,
}

/// A mismatch is affirmative refusal; unavailable proof never grants custody and is not
/// evidence that the caller is secondary. Diagnostics contain no supplied path or payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureKind {
    Refused,
    Unavailable,
}

#[derive(Debug)]
struct Failure {
    kind: FailureKind,
    reason: &'static str,
}
impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.reason)
    }
}
impl std::error::Error for Failure {}
fn refused(reason: &'static str) -> Failure {
    Failure {
        kind: FailureKind::Refused,
        reason,
    }
}
fn unavailable(error: anyhow::Error) -> anyhow::Error {
    error.context(Failure {
        kind: FailureKind::Unavailable,
        reason: "Claude process proof is unavailable",
    })
}
pub fn failure_kind(error: &anyhow::Error) -> Option<FailureKind> {
    // anyhow's own downcast traverses typed context; std::error::Error::source
    // does not expose the context value itself as a downcastable Failure.
    error
        .downcast_ref::<Failure>()
        .map(|failure| failure.kind)
        .or_else(|| {
            error.chain().find_map(|cause| {
                cause.downcast_ref::<Failure>().map(|failure| failure.kind)
            })
        })
}
pub fn diagnostic(error: &anyhow::Error) -> &'static str {
    match failure_kind(error) {
        Some(FailureKind::Refused) => "Claude ownership refused: process or generation mismatch",
        _ => "Claude ownership unavailable after a bounded recheck; publication withheld",
    }
}

// The new proof adds no blocking flock. A transient unavailable capture gets at most one
// recheck, before the 25ms proof budget expires, with 5ms maximum intentional waiting.
// This bounds added lock waiting, not OS filesystem/proc latency or existing action work.
const PROOF_BUDGET: Duration = Duration::from_millis(25);
const RECHECK_PAUSE: Duration = Duration::from_millis(5);
fn proof_with_recheck<T>(capture: impl FnMut() -> Result<T>) -> Result<T> {
    let start = Instant::now();
    proof_with_recheck_clock(
        capture,
        || start.elapsed(),
        || std::thread::sleep(RECHECK_PAUSE),
    )
}
fn proof_with_recheck_clock<T>(
    mut capture: impl FnMut() -> Result<T>,
    elapsed: impl Fn() -> Duration,
    pause: impl FnOnce(),
) -> Result<T> {
    match capture() {
        Err(error)
            if failure_kind(&error) == Some(FailureKind::Unavailable)
                && elapsed().saturating_add(RECHECK_PAUSE) < PROOF_BUDGET =>
        {
            pause();
            if elapsed() >= PROOF_BUDGET {
                return Err(error);
            }
            capture()
        }
        result => result,
    }
}

pub fn recorded(dir: &Path) -> Result<bool> {
    Ok(dir.join(FILE).try_exists()?)
}

pub fn wrapper(dir: &Path) -> Result<Process> {
    Ok(serde_json::from_slice::<Owner>(&read_owner(dir)?)?.wrapper)
}

fn read_owner(dir: &Path) -> Result<Vec<u8>> {
    use std::io::Read as _;
    let file = fs::File::open(dir.join(FILE))?;
    anyhow::ensure!(
        file.metadata()?.is_file(),
        refused("Claude witness is not a regular file")
    );
    let mut bytes = Vec::new();
    file.take(4097).read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() <= 4096,
        refused("Claude witness exceeds the byte limit")
    );
    Ok(bytes)
}

fn lock(dir: &Path, create: bool) -> Result<crate::flock::FileLock> {
    use crate::flock::{self, FileLock, Mode, Open};
    let file = flock::open(
        &dir.join(LOCK),
        if create { Open::Create } else { Open::Existing },
    )?;
    Ok(FileLock::hold_blocking(file, Mode::Exclusive)?)
}

fn try_lock(dir: &Path) -> Result<crate::flock::FileLock> {
    use crate::flock::{self, FileLock, Mode, Open, Wait};
    let file = flock::open(&dir.join(LOCK), Open::Existing)?;
    FileLock::hold(file, Mode::Exclusive, Wait::Now)?
        .ok_or_else(|| unavailable(std::io::Error::from(std::io::ErrorKind::WouldBlock).into()))
}

fn write(dir: &Path, owner: &Owner) -> Result<()> {
    crate::harness_state::write_json_atomic(&dir.join(FILE), owner, dir, ".claude-primary-process")
}

/// Apply the owning install's default-off choice at a FRESH wrapper claim only. Adoption
/// never calls this. Off preserves the original harness claim and does no kernel proof.
pub(crate) fn claim_configured(
    dir: &Path,
    identity: &str,
    runtime: &str,
    session: &str,
    enabled: bool,
) -> Result<u64> {
    if enabled {
        return claim(dir, identity, runtime, session);
    }
    if !recorded(dir)? {
        return crate::harness_state::claim(dir, identity, "claude", session);
    }
    // A previous protected generation may have left a closed witness. A newly claimed off
    // generation must not inherit its restriction. Fence old publications with the ordinary
    // harness claim before retiring that witness, under the same ownership lock. Never unlink
    // the lock inode. This runs only at an authorized new wrapper launch, not on config reload.
    let _guard = lock(dir, true)?;
    let sequence = crate::harness_state::claim(dir, identity, "claude", session)?;
    match fs::remove_file(dir.join(FILE)) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(sequence)
}

/// Claim the harness and record its actual wrapper generation BEFORE spawning the provider.
/// Lock order is process-owner, then harness-state; helper publications use the same order.
pub(crate) fn claim(dir: &Path, identity: &str, runtime: &str, session: &str) -> Result<u64> {
    // Before the early SessionStart window, assert that this wrapper cannot adopt orphaned
    // nested descendants as direct children. No production path sets CHILD_SUBREAPER; a future
    // caller changing that must settle this protocol rather than silently widening its proof.
    ensure_no_subreaper(subreaper()?)?;
    fs::create_dir_all(dir)?;
    let _guard = lock(dir, true)?;
    let wrapper = process(std::process::id())?;
    let sequence = crate::harness_state::claim(dir, identity, "claude", session)?;
    write(
        dir,
        &Owner {
            version: 1,
            session: session.into(),
            sequence,
            runtime: runtime.into(),
            wrapper,
            provider: None,
            closed: false,
        },
    )?;
    Ok(sequence)
}

/// The supervisor's spawned PID is authoritative, never a helper-provided value. An early
/// SessionStart may precede this write: only the kernel-proven direct child of this wrapper can
/// publish during that window. The wrapper performs no other spawns after claim and before this.
pub(crate) fn spawned(dir: &Path, session: &str, sequence: u64, pid: u32) -> Result<()> {
    if !recorded(dir)? {
        return Ok(()); // Default-off/legacy generation: no new ownership proof or record.
    }
    let _guard = lock(dir, false)?;
    let mut owner: Owner = serde_json::from_slice(&read_owner(dir)?)?;
    anyhow::ensure!(
        owner.session == session && owner.sequence == sequence && !owner.closed,
        "Claude process ownership was superseded"
    );
    anyhow::ensure!(
        process(std::process::id())? == owner.wrapper,
        "Claude wrapper generation changed"
    );
    let child = process(pid)?;
    anyhow::ensure!(
        child.parent == owner.wrapper.pid,
        "Claude provider is not the wrapper's child"
    );
    owner.provider = Some(child);
    write(dir, &owner)
}

pub(crate) fn ended(dir: &Path, session: &str, sequence: u64) -> Result<()> {
    if !recorded(dir)? {
        return Ok(());
    } // A reexec of a legacy wrapper stays legacy.
    let _guard = lock(dir, false)?;
    let mut owner: Owner = serde_json::from_slice(&read_owner(dir)?)?;
    if owner.session == session && owner.sequence == sequence {
        owner.closed = true;
        write(dir, &owner)?;
    }
    Ok(())
}

/// A daemon supplies its independently captured physical wrapper. Local hooks have no daemon
/// connection, but still require the exact wrapper session/sequence and kernel relationship.
/// A missing record is explicitly legacy, never an implicit primary proof. Malformed records
/// and records for a different generation fail closed; they cannot downgrade to legacy.
#[allow(clippy::too_many_arguments)]
pub fn with_caller<T>(
    dir: &Path,
    session: &str,
    sequence: u64,
    pid: u32,
    expected_wrapper: Option<Process>,
    role: Caller<'_>,
    action: impl FnOnce(Admission) -> Result<T>,
) -> Result<T> {
    let mut protected = expected_wrapper.is_some();
    let (admission, _guard) = proof_with_recheck(|| {
        let path = dir.join(FILE);
        // Only proven absence is legacy. Helpers never create either record or lock.
        if !path.try_exists().map_err(|e| unavailable(e.into()))? {
            anyhow::ensure!(!protected, refused("protected Claude witness disappeared"));
            return Ok((Admission::Legacy, None));
        }
        // A recheck must not turn disappearance of a witnessed generation into legacy.
        protected = true;
        let guard = try_lock(dir).map_err(unavailable)?;
        let bytes = read_owner(dir).map_err(classify_read)?;
        let owner: Owner = serde_json::from_slice(&bytes)
            .map_err(|e| anyhow::Error::new(e).context(refused("malformed Claude witness")))?;
        anyhow::ensure!(
            owner.version == 1
                && !owner.closed
                && owner.session == session
                && owner.sequence == sequence,
            refused("Claude primary ownership is closed or superseded")
        );
        if let Some(expected) = expected_wrapper {
            anyhow::ensure!(
                expected == owner.wrapper,
                refused("Claude ownership disagrees with the physical wrapper")
            );
        }
        let current = crate::harness_state::try_current_ownership(dir, session, sequence)
            .map_err(classify_read)?
            .ok_or_else(|| {
                unavailable(std::io::Error::from(std::io::ErrorKind::WouldBlock).into())
            })?;
        anyhow::ensure!(current, refused("harness ownership was superseded"));
        check_processes(&owner, &bytes, pid, &role, process, arguments, || {
            read_owner(dir)
        })?;
        Ok((Admission::Primary, Some(guard)))
    })?;
    // Publication is outside the retry: an action error can never replay a write or binding.
    action(admission)
}

fn classify_read(error: anyhow::Error) -> anyhow::Error {
    if failure_kind(&error).is_some() {
        error
    } else if error.chain().any(|e| e.is::<std::io::Error>()) {
        unavailable(error)
    } else {
        error.context(refused("Claude ownership record is malformed"))
    }
}

fn check_processes(
    owner: &Owner,
    bytes: &[u8],
    pid: u32,
    role: &Caller<'_>,
    read: impl Fn(u32) -> Result<Process>,
    args: impl Fn(u32) -> Result<Vec<String>>,
    reread: impl FnOnce() -> Result<Vec<u8>>,
) -> Result<()> {
    let proof = prove(owner, pid, role, &read, args)?;
    validate_proof(&proof, read)?;
    anyhow::ensure!(
        reread().map_err(classify_read)? == bytes,
        refused("Claude process witness changed during admission")
    );
    Ok(())
}

// macOS reparents orphans to launchd, never this wrapper. Linux subreaper is per-process and
// not inherited by forked wrappers. Reexec retains it, so every new claim checks it again.
#[cfg(target_os = "linux")]
fn subreaper() -> Result<bool> {
    let mut value: libc::c_int = 0;
    // SAFETY: PR_GET_CHILD_SUBREAPER writes one c_int into a live local value.
    let result = unsafe {
        libc::prctl(
            libc::PR_GET_CHILD_SUBREAPER,
            &mut value as *mut libc::c_int,
            0,
            0,
            0,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(value != 0)
}
#[cfg(not(target_os = "linux"))]
fn subreaper() -> Result<bool> {
    Ok(false)
}
fn ensure_no_subreaper(value: bool) -> Result<()> {
    anyhow::ensure!(
        !value,
        refused("Claude wrapper must not adopt orphaned descendants")
    );
    Ok(())
}

fn validate_proof(proof: &[Process], read: impl Fn(u32) -> Result<Process>) -> Result<()> {
    for captured in proof {
        anyhow::ensure!(
            read(captured.pid).map_err(unavailable)? == *captured,
            refused("Claude process parent or birth changed during admission")
        );
    }
    Ok(())
}

fn prove(
    owner: &Owner,
    pid: u32,
    role: &Caller<'_>,
    read: impl Fn(u32) -> Result<Process>,
    args: impl Fn(u32) -> Result<Vec<String>>,
) -> Result<Vec<Process>> {
    let wrapper = read(owner.wrapper.pid).map_err(unavailable)?;
    anyhow::ensure!(
        wrapper == owner.wrapper && wrapper.pid > 1,
        refused("Claude wrapper generation is not live")
    );
    let caller = read(pid).map_err(unavailable)?;
    if matches!(role, Caller::Wrapper) {
        anyhow::ensure!(
            caller == wrapper,
            refused("this is not the primary Claude wrapper")
        );
        return Ok(vec![wrapper]);
    }
    let mut proof = vec![wrapper, caller];
    let mut provider = read(caller.parent).map_err(unavailable)?;
    if let Caller::Hook { trampoline } = role {
        let argv = args(provider.pid).map_err(unavailable)?;
        if exact_trampoline(&argv, trampoline) {
            proof.push(provider);
            provider = read(provider.parent).map_err(unavailable)?;
        }
    }
    anyhow::ensure!(
        provider.pid > 1 && provider.parent == wrapper.pid,
        refused("Claude process chain is unsupported by direct-parent ownership")
    );
    if let Some(expected) = owner.provider {
        anyhow::ensure!(
            provider == expected,
            refused("Claude primary provider generation changed")
        );
    }
    proof.push(provider);
    Ok(proof)
}

/// Read-only direct-chain SHAPE snapshot for a legacy daemon diagnostic. Without a provider
/// witness this does not prove provider/session ownership and must never authorize anything.
pub fn physical_chain(wrapper: Process, pid: u32, role: Caller<'_>) -> Result<Vec<Process>> {
    let owner = Owner {
        version: 1,
        session: String::new(),
        sequence: 0,
        runtime: String::new(),
        wrapper,
        provider: None,
        closed: false,
    };
    let proof = prove(&owner, pid, &role, process, arguments)?;
    validate_proof(&proof, process)?;
    Ok(proof)
}

fn exact_trampoline(argv: &[String], command: &str) -> bool {
    argv.len() == 3
        && argv[1] == "-c"
        && argv[2] == command
        && matches!(
            Path::new(&argv[0]).file_name().and_then(|p| p.to_str()),
            Some("sh" | "bash" | "dash" | "zsh")
        )
}

#[cfg(target_os = "linux")]
pub fn process(pid: u32) -> Result<Process> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let (_, tail) = stat.rsplit_once(") ").context("malformed process stat")?;
    let fields: Vec<_> = tail.split_whitespace().collect();
    anyhow::ensure!(
        fields.first().is_some_and(|s| !matches!(*s, "Z" | "X")),
        "process is terminal"
    );
    Ok(Process {
        pid,
        parent: fields.get(1).context("missing process parent")?.parse()?,
        birth: fields.get(19).context("missing process birth")?.parse()?,
    })
}

#[cfg(target_os = "macos")]
pub fn process(pid: u32) -> Result<Process> {
    let pid_arg = i32::try_from(pid)?;
    let mut info = std::mem::MaybeUninit::<libc::proc_taskallinfo>::zeroed();
    let size = std::mem::size_of::<libc::proc_taskallinfo>() as libc::c_int;
    let count = unsafe {
        libc::proc_pidinfo(
            pid_arg,
            libc::PROC_PIDTASKALLINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    anyhow::ensure!(count == size, "Darwin process generation is unavailable");
    let info = unsafe { info.assume_init() }.pbsd;
    anyhow::ensure!(
        info.pbi_pid == pid && info.pbi_status != 5,
        "Darwin process is terminal"
    );
    Ok(Process {
        pid,
        parent: info.pbi_ppid,
        birth: info
            .pbi_start_tvsec
            .checked_mul(1_000_000)
            .and_then(|v| v.checked_add(info.pbi_start_tvusec))
            .context("Darwin process birth overflow")?,
    })
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn process(_pid: u32) -> Result<Process> {
    anyhow::bail!("process ownership is unsupported")
}

#[cfg(target_os = "linux")]
fn arguments(pid: u32) -> Result<Vec<String>> {
    use std::io::Read as _;
    let mut bytes = Vec::new();
    fs::File::open(format!("/proc/{pid}/cmdline"))?
        .take(1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() <= 1024 * 1024,
        "process arguments exceed the limit"
    );
    Ok(bytes
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect())
}

#[cfg(target_os = "macos")]
fn arguments(pid: u32) -> Result<Vec<String>> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, i32::try_from(pid)?];
    let mut size = 0;
    let result = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    anyhow::ensure!(
        result == 0 && size <= 1024 * 1024,
        "process arguments unavailable"
    );
    let mut bytes = vec![0u8; size];
    let result = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            bytes.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    anyhow::ensure!(result == 0, "process arguments changed");
    bytes.truncate(size);
    let argc = i32::from_ne_bytes(bytes.get(..4).context("missing argc")?.try_into()?);
    let tail = bytes.get(4..).context("missing argument buffer")?;
    let tail = tail
        .get(
            tail.iter()
                .position(|b| *b == 0)
                .context("missing executable")?
                + 1..,
        )
        .context("missing arguments")?;
    let padding = tail.iter().take_while(|b| **b == 0).count();
    let mut parts = tail[padding..].split(|b| *b == 0);
    (0..usize::try_from(argc)?)
        .map(|_| {
            parts
                .next()
                .map(|p| String::from_utf8_lossy(p).into_owned())
                .context("truncated process arguments")
        })
        .collect()
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn arguments(_pid: u32) -> Result<Vec<String>> {
    anyhow::bail!("process arguments unsupported")
}

#[cfg(test)]
mod tests;
