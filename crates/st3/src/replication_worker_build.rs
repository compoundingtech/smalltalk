//! Same-host worker identity is operational metadata, never a replicated claim.
use std::{fs, path::Path};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

use crate::model::DoctorCheck;

const FILE: &str = "replication-worker-build.json";

#[derive(Deserialize, Serialize)]
struct WorkerBuild {
    pid: u32,
    start_token: u64,
    machine_version: String,
}

/// Publish before listening so an independently restarted daemon still sees the worker's build.
pub(crate) fn record(state_dir: &Path) -> Result<()> {
    let pid = std::process::id();
    let report = WorkerBuild {
        pid,
        start_token: st_runtime::process_start_token(pid)?,
        machine_version: st_drivers::version::machine_version(),
    };
    let pending = state_dir.join(format!("{FILE}.{}.tmp", report.pid));
    fs::write(&pending, serde_json::to_vec(&report)?)
        .context("write replication worker build report")?;
    fs::rename(&pending, state_dir.join(FILE))
        .context("publish replication worker build report")
}

pub(crate) fn warning(worker: &str, daemon: Option<&str>) -> Option<String> {
    match daemon {
        Some(daemon) if daemon == worker => None,
        Some(daemon) => Some(format!(
            "replication worker build {worker} differs from same-host daemon build {daemon}; \
             deploy the same st build to both services: an older worker can omit replication fixes"
        )),
        None => Some(format!(
            "cannot compare replication worker build {worker} with the same-host daemon: \
             daemon health reports no machine_version; deploy the same st build to both services"
        )),
    }
}

pub(crate) fn check(state_dir: &Path) -> DoctorCheck {
    let report = fs::read(state_dir.join(FILE))
        .map_err(anyhow::Error::from)
        .and_then(|bytes| serde_json::from_slice::<WorkerBuild>(&bytes).map_err(Into::into));
    let (status, message) = match report {
        Ok(report) if worker_running(&report) => {
            let daemon = st_drivers::version::machine_version();
            match warning(&report.machine_version, Some(&daemon)) {
                Some(message) => ("warn", message),
                None => ("pass", format!(
                    "replication worker PID {} and same-host daemon run {daemon}", report.pid
                )),
            }
        }
        Ok(report) => ("warn", format!(
            "replication worker build report names PID {}, which is no longer that running process; \
             worker build is unknown", report.pid
        )),
        Err(error) => ("warn", format!(
            "replication worker build is unknown ({error}); older workers do not report their build; \
             deploy the same st build to daemon and replication worker"
        )),
    };
    DoctorCheck {
        name: "replication-worker-build".into(),
        status: status.into(),
        message,
    }
}

fn worker_running(report: &WorkerBuild) -> bool {
    // Signal zero probes existence without sending a signal. Reject process-group sentinels.
    let Ok(pid) = i32::try_from(report.pid) else { return false; };
    if pid <= 1 { return false; }
    let exists = (unsafe { libc::kill(pid, 0) == 0 })
        || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
    exists && st_runtime::process_start_token(report.pid)
        .is_ok_and(|token| token == report.start_token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mismatch_and_unreported_daemon_build_warn_but_matching_build_does_not() {
        let mismatch = warning("0.1.0+old", Some("0.1.0+new")).unwrap();
        assert!(mismatch.contains("0.1.0+old") && mismatch.contains("0.1.0+new"));
        assert!(warning("0.1.0+old", None).unwrap().contains("0.1.0+old"));
        assert_eq!(warning("0.1.0+same", Some("0.1.0+same")), None);
    }

    #[test]
    fn doctor_distinguishes_live_mismatch_missing_report_and_dead_worker() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(check(root.path()).status, "warn");
        record(root.path()).unwrap();
        assert_eq!(check(root.path()).status, "pass");
        let mut report: WorkerBuild = serde_json::from_slice(
            &fs::read(root.path().join(FILE)).unwrap()
        ).unwrap();
        report.machine_version = "0.1.0+old".into();
        fs::write(root.path().join(FILE), serde_json::to_vec(&report).unwrap()).unwrap();
        let mismatch = check(root.path());
        assert_eq!(mismatch.status, "warn");
        assert!(mismatch.message.contains("0.1.0+old"));
        report.pid = 0;
        fs::write(root.path().join(FILE), serde_json::to_vec(&report).unwrap()).unwrap();
        assert_eq!(check(root.path()).status, "warn");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn reused_pid_cannot_make_a_stale_worker_report_pass() {
        let root = tempfile::tempdir().unwrap();
        record(root.path()).unwrap();
        let mut report: WorkerBuild = serde_json::from_slice(
            &fs::read(root.path().join(FILE)).unwrap()
        ).unwrap();
        report.start_token = report.start_token.wrapping_add(1);
        fs::write(root.path().join(FILE), serde_json::to_vec(&report).unwrap()).unwrap();
        let stale = check(root.path());
        assert_eq!(stale.status, "warn");
    }
}
