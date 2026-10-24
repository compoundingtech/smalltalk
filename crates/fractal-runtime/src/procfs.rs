//! Local PIDs are safe procfs keys only when procfs belongs to our PID namespace.

use anyhow::Result;

/// Process identity cannot be established in a procfs mounted for another PID namespace.
/// This is a refusal, not evidence that a process has exited.
#[derive(Debug)]
pub struct ProcessNamespaceMismatch {
    pub detail: &'static str,
    pub process_pid: u32,
    pub procfs_pid: Option<u32>,
}

impl std::fmt::Display for ProcessNamespaceMismatch {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "process namespace identity unavailable ({}; local PID {}; procfs PID {:?})",
            self.detail, self.process_pid, self.procfs_pid
        )
    }
}

impl std::error::Error for ProcessNamespaceMismatch {}

#[cfg(target_os = "linux")]
pub(crate) fn require_matching_procfs() -> Result<()> {
    let pid = std::process::id();
    let refuse = |detail, procfs_pid| ProcessNamespaceMismatch {
        detail,
        process_pid: pid,
        procfs_pid,
    };
    let target = std::fs::read_link("/proc/self")
        .map_err(|_| refuse("proc-self-unreadable", None))?;
    let procfs_pid = target
        .to_str()
        .and_then(|value| value.parse::<u32>().ok())
        .ok_or_else(|| refuse("proc-self-invalid", None))?;
    let status = std::fs::read_to_string("/proc/self/status")
        .map_err(|_| refuse("proc-status-unreadable", Some(procfs_pid)))?;
    validate_namespace(pid, procfs_pid, &status).map_err(Into::into)
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn require_matching_procfs() -> Result<()> {
    Ok(())
}

#[cfg(target_os = "linux")]
fn validate_namespace(
    pid: u32,
    procfs_pid: u32,
    status: &str,
) -> std::result::Result<(), ProcessNamespaceMismatch> {
    let refuse = |detail| ProcessNamespaceMismatch {
        detail,
        process_pid: pid,
        procfs_pid: Some(procfs_pid),
    };
    if procfs_pid != pid {
        return Err(refuse("proc-self-pid-mismatch"));
    }
    let mut lines = status.lines().filter_map(|line| line.strip_prefix("NSpid:"));
    let line = lines.next().ok_or_else(|| refuse("nspid-unavailable"))?;
    if lines.next().is_some() {
        return Err(refuse("nspid-invalid"));
    }
    let mut ids = line.split_whitespace();
    let first = ids
        .next()
        .and_then(|value| value.parse::<u32>().ok())
        .ok_or_else(|| refuse("nspid-invalid"))?;
    // NSpid starts at the procfs mount's namespace, not at the process's namespace.
    // Merely comparing its LAST entry to getpid() would accept the broken layout.
    // Exactly one entry also rejects the accidental equal-number alias case.
    if first != pid || ids.next().is_some() {
        return Err(refuse("proc-status-pid-namespace-mismatch"));
    }
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn matching_procfs_namespace_accepts_a_single_local_pid() {
        validate_namespace(7, 7, "Name:\tworker\nNSpid:\t7\n").unwrap();
    }

    #[test]
    fn inherited_procfs_namespace_refuses_even_when_last_nspid_matches() {
        let error = validate_namespace(7, 41, "NSpid:\t41\t7\n").unwrap_err();
        assert_eq!(error.detail, "proc-self-pid-mismatch");
        assert_eq!(error.procfs_pid, Some(41));
    }

    #[test]
    fn accidental_equal_pid_does_not_make_an_outer_procfs_safe() {
        let error = validate_namespace(7, 7, "NSpid:\t7\t7\n").unwrap_err();
        assert_eq!(error.detail, "proc-status-pid-namespace-mismatch");
    }

    #[test]
    fn missing_or_invalid_namespace_metadata_refuses() {
        for (status, detail) in [
            ("Pid:\t7\n", "nspid-unavailable"),
            ("NSpid:\n", "nspid-invalid"),
            ("NSpid:\tx\n", "nspid-invalid"),
            ("NSpid:\t7\nNSpid:\t7\n", "nspid-invalid"),
            ("NSpid:\t8\n", "proc-status-pid-namespace-mismatch"),
        ] {
            assert_eq!(validate_namespace(7, 7, status).unwrap_err().detail, detail);
        }
    }
}
