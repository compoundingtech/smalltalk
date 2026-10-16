//! Durable admission for this machine's GitHub observer requests.
//!
//! This is a local request ceiling, not a reservation of a shared token's quota.
//! No other machine is consulted. Failed sends and conditional 304s retain their
//! reservation. GraphQL requests are counted separately from core requests; their
//! provider point cost is not inferred from request count.
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Read as _, Write as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use anyhow::{Context as _, Result, ensure};
use serde::{Deserialize, Serialize};

const HOUR: u128 = 3_600_000;
#[cfg(test)]
const CAP: usize = 1000;
const MINUTE: u128 = 60_000;
const DAY: u128 = 24 * HOUR;
const MAX_BYTES: u64 = 4 * 1024 * 1024;
const MAX_ROWS: usize = 32_000;

/// GitHub refused a request until a known time: a primary or secondary rate limit. It clears
/// on its own at `retry_at_unix_ms`.
#[derive(Debug)]
pub struct ProviderRateLimit {
    pub retry_at_unix_ms: u128,
    pub status: u16,
}

impl std::fmt::Display for ProviderRateLimit {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "GitHub HTTP {} rate limit", self.status)
    }
}

impl std::error::Error for ProviderRateLimit {}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Reservation {
    at: u128,
    caller: String,
    resource: String,
    observer: bool,
    count: u64,
}

#[derive(Default)]
struct State {
    path: Option<std::path::PathBuf>,
    rows: Vec<Reservation>,
    refused: BTreeMap<(String, String), u64>,
    gap: Option<String>,
    limit: Option<usize>,
}

fn state() -> &'static Mutex<State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE.get_or_init(Default::default)
}

/// Load durable counters at daemon configuration, so doctor does not wait for a
/// new request to show prior-hour reservations. A corrupt ledger stays a visible
/// gap and refuses observer sends; startup does not erase it or stop merge paths.
pub fn configure(path: std::path::PathBuf, limit: Option<usize>) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|time| time.as_millis())
        .unwrap_or(0);
    if let Ok(mut held) = state().lock() {
        held.limit = limit;
        cleanup_temporary(&path);
        match read(&path, now) {
            Ok(rows) => {
                held.rows = rows;
                held.path = Some(path);
                held.gap = None;
            }
            Err(error) => {
                held.path = None;
                held.gap = Some(error.to_string());
            }
        }
    }
}

fn read(path: &Path, now: u128) -> Result<Vec<Reservation>> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).context("open GitHub admission ledger"),
    };
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.len() <= MAX_BYTES,
        "invalid GitHub admission ledger file"
    );
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_BYTES,
        "GitHub admission ledger exceeds byte bound"
    );
    let rows: Vec<Reservation> = serde_json::from_slice(&bytes)
        .context("decode GitHub admission ledger; no empty-ledger recovery was used")?;
    ensure!(
        rows.len() <= MAX_ROWS,
        "GitHub admission ledger exceeds row bound"
    );
    ensure!(
        rows.iter().all(|r| r.at <= now.saturating_add(DAY)
            && !r.caller.is_empty()
            && r.caller.len() <= 512
            && r.count > 0
            && matches!(r.resource.as_str(), "core" | "graphql" | "search")),
        "invalid GitHub admission reservation"
    );
    Ok(rows
        .into_iter()
        .map(|mut r| {
            r.at = r.at.min(now);
            r
        })
        .filter(|r| now.saturating_sub(r.at) < HOUR + MINUTE)
        .collect())
}

fn publish(path: &Path, rows: &[Reservation]) -> Result<()> {
    ensure!(
        rows.len() <= MAX_ROWS,
        "GitHub admission ledger exceeds row bound"
    );
    let bytes = serde_json::to_vec(rows)?;
    ensure!(
        bytes.len() as u64 <= MAX_BYTES,
        "GitHub admission ledger exceeds byte bound"
    );
    let parent = path
        .parent()
        .context("GitHub admission ledger has no parent")?;
    // State is already the daemon's private configured directory. A unique,
    // no-replace temporary file prevents concurrent writers from sharing bytes.
    let temporary = parent.join(format!(".github-admission-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    // Only our unique temporary name is removed. A rename already committed the
    // reservation even if the parent fsync failed; in that case no send follows.
    if temporary.exists() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

fn cleanup_temporary(path: &Path) {
    use std::os::unix::fs::MetadataExt as _;
    let Some(parent) = path.parent() else { return };
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    // Configuration occurs before this daemon sends; this private state has one
    // daemon writer. Only our private regular-file temporaries are eligible.
    for entry in entries.take(1024).flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(".github-admission-")
            && name.ends_with(".tmp")
            && let Ok(meta) = entry.path().symlink_metadata()
            && meta.is_file()
            && meta.uid() == unsafe { libc::geteuid() }
        {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn admit(
    rows: &mut Vec<Reservation>,
    caller: &str,
    resource: &str,
    observer: bool,
    now: u128,
    limit: Option<usize>,
) -> Result<()> {
    ensure!(
        !caller.is_empty() && caller.len() <= 512,
        "invalid GitHub admission caller"
    );
    rows.retain(|r| now.saturating_sub(r.at) < HOUR + MINUTE);
    let relevant = rows
        .iter()
        .filter(|r| r.observer && r.resource == resource)
        .collect::<Vec<_>>();
    let used = relevant.iter().map(|r| u128::from(r.count)).sum::<u128>();
    let caller_used = relevant
        .iter()
        .filter(|r| r.caller == caller)
        .map(|r| u128::from(r.count))
        .sum::<u128>();
    if observer && limit.is_some_and(|cap| used >= cap as u128 || caller_used >= cap as u128) {
        let retry_at = relevant
            .iter()
            .map(|r| r.at)
            .min()
            .unwrap_or(now)
            .saturating_add(HOUR + MINUTE);
        anyhow::bail!(ProviderRateLimit {
            retry_at_unix_ms: retry_at,
            status: 429
        });
    }
    let minute = now / MINUTE * MINUTE;
    if let Some(row) = rows.iter_mut().find(|r| {
        r.at == minute && r.caller == caller && r.resource == resource && r.observer == observer
    }) {
        row.count = row
            .count
            .checked_add(1)
            .context("GitHub admission bucket overflow")?;
    } else {
        rows.push(Reservation {
            at: minute,
            caller: caller.into(),
            resource: resource.into(),
            observer,
            count: 1,
        });
    }
    Ok(())
}

/// Called before BOTH local reqwest and authorized RPC sends. A task-local
/// observer subject is supplied by the reconciler, never from an HTTP header or
/// the shell's ST_AGENT. Unscoped calls are visible but not capped here: comments
/// and live merge gates need their own authenticated caller enrollment before
/// their write path may be throttled. They must not inherit an observer identity.
pub async fn before_send(
    path: std::path::PathBuf,
    caller: Option<String>,
    resource: String,
    limit: Option<usize>,
) -> Result<()> {
    let observer = caller.is_some();
    let enforcing = observer && limit.is_some();
    let caller = caller.unwrap_or_else(|| "unattributed".into());
    let result = tokio::task::spawn_blocking(move || {
        let mut held = state()
            .lock()
            .map_err(|_| anyhow::anyhow!("GitHub admission mutex poisoned"))?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .context("GitHub admission clock precedes epoch")?
            .as_millis();
        held.limit = limit;
        let result: Result<()> = (|| {
            if held.path.as_ref() != Some(&path) {
                held.rows = read(&path, now)?;
                held.path = Some(path.clone());
            }
            let mut rows = held.rows.clone();
            admit(&mut rows, &caller, &resource, observer, now, limit)?;
            publish(&path, &rows)?;
            held.rows = rows;
            held.gap = None;
            Ok(())
        })();
        if let Err(error) = &result {
            // A rename may have committed before a directory fsync error. Force
            // the next admission to reload, so it never overwrites spent usage.
            held.path = None;
            *held
                .refused
                .entry((caller.clone(), resource.clone()))
                .or_default() += 1;
            held.gap = Some(error.to_string());
            // A merge-path request with no enrolled observer caller is never
            // refused because the observer's durable meter is broken.
            if !enforcing {
                tracing::warn!(
                    "GitHub meter-only or unscoped request admitted with meter gap: {error}"
                );
                return Ok(());
            }
        }
        result
    })
    .await
    .context("GitHub admission worker")
    .and_then(|result| result);
    if !enforcing && result.is_err() {
        tracing::warn!("GitHub unscoped request admitted with unavailable admission worker");
        return Ok(());
    }
    result
}

pub fn doctor_lines(now: u128) -> Vec<crate::model::DoctorCheck> {
    // Doctor must not wait for the journal writer's disk sync.
    let Ok(held) = state().try_lock() else {
        return vec![crate::model::DoctorCheck {
            name: "github-requests/admission".into(),
            status: "warn".into(),
            message: "local admission state unavailable".into(),
        }];
    };
    let mut counts = BTreeMap::<(String, String), u128>::new();
    for r in &held.rows {
        if now.saturating_sub(r.at) < HOUR + MINUTE {
            *counts
                .entry((r.caller.clone(), r.resource.clone()))
                .or_default() += u128::from(r.count);
        }
    }
    for key in held.refused.keys() {
        counts.entry(key.clone()).or_default();
    }
    let omitted_groups = counts.len().saturating_sub(127);
    let omitted_count = counts.values().skip(127).copied().sum::<u128>();
    let mut lines = counts.into_iter().take(127).map(|((caller, resource), count)| crate::model::DoctorCheck {
        name: format!("github-requests/admitted/{caller}/{resource}"),
        status: if held.gap.is_some() { "warn" } else { "pass" }.into(),
        message: format!("{count} durable request reservations in the conservative61-minute window (including failed sends and 304s); {} admission refusals/meter failures since start; per-minute buckets retain up to61minutes conservatively; observer aggregate/per-caller limit {:?}/{resource}/hour (None=meter-only); GraphQL points unknown; unscoped callers metered but uncapped", held.refused.get(&(caller.clone(),resource.clone())).copied().unwrap_or(0), held.limit),
    }).collect::<Vec<_>>();
    if omitted_groups > 0 {
        lines.push(crate::model::DoctorCheck {
            name: "github-requests/admitted/other".into(), status: "pass".into(),
            message: format!("{omitted_count} reservations across {omitted_groups} additional caller/resource groups; full journal retained"),
        });
    }
    if let Some(gap) = &held.gap {
        lines.push(crate::model::DoctorCheck {
            name: "github-requests/admission".into(),
            status: "warn".into(),
            message: gap.clone(),
        });
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restart_retains_spent_requests_and_refusal_does_not_append() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("admission.json");
        let now = 10_000_000;
        let mut rows = Vec::new();
        for _ in 0..CAP {
            admit(
                &mut rows,
                "observer/github/repo",
                "core",
                true,
                now,
                Some(CAP),
            )
            .unwrap();
        }
        publish(&path, &rows).unwrap();
        let mut restarted = read(&path, now).unwrap();
        assert!(
            admit(
                &mut restarted,
                "observer/github/repo",
                "core",
                true,
                now,
                Some(CAP)
            )
            .is_err()
        );
        assert_eq!(restarted.iter().map(|r| r.count).sum::<u64>(), CAP as u64);
        // A different caller cannot acquire a second aggregate1000 allocation.
        assert!(
            admit(
                &mut restarted,
                "observer/github/another",
                "core",
                true,
                now,
                Some(CAP)
            )
            .is_err()
        );
        admit(
            &mut restarted,
            "observer/github/repo",
            "graphql",
            true,
            now,
            Some(CAP),
        )
        .unwrap();
        admit(
            &mut restarted,
            "unattributed",
            "core",
            false,
            now,
            Some(CAP),
        )
        .unwrap();
        admit(
            &mut restarted,
            "observer/github/repo",
            "core",
            true,
            now + HOUR + MINUTE,
            Some(CAP),
        )
        .unwrap();
        assert_eq!(restarted.len(), 1);
    }

    #[tokio::test]
    async fn exhausted_observer_reservation_returns_existing_rate_limit_without_spending() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("admission.json");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let rows = (0..CAP)
            .map(|_| Reservation {
                at: now,
                caller: "observer/github/repo".into(),
                resource: "core".into(),
                observer: true,
                count: 1,
            })
            .collect::<Vec<_>>();
        publish(&path, &rows).unwrap();
        let before = std::fs::read(&path).unwrap();
        let error = before_send(
            path.clone(),
            Some("observer/github/repo".into()),
            "core".into(),
            Some(CAP),
        )
        .await
        .unwrap_err();
        assert!(
            error
                .downcast_ref::<ProviderRateLimit>()
                .is_some()
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[tokio::test]
    async fn failed_send_budget_remains_on_disk_without_a_response() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("admission.json");
        before_send(
            path.clone(),
            Some("observer/github/repo".into()),
            "graphql".into(),
            Some(CAP),
        )
        .await
        .unwrap();
        // No response-recording call occurs. Reopening still sees the admitted request.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let rows = read(&path, now).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].resource, "graphql");
    }

    #[test]
    fn unscoped_flood_is_one_bucket_and_cannot_consume_observer_room() {
        let mut rows = Vec::new();
        let now = 10_000_000;
        for _ in 0..100_000 {
            admit(&mut rows, "unattributed", "core", false, now, Some(CAP)).unwrap();
        }
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].count, 100_000);
        admit(
            &mut rows,
            "observer/github/repo",
            "core",
            true,
            now,
            Some(CAP),
        )
        .unwrap();
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn small_backward_clock_step_retains_usage_and_unset_limit_only_meters() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("ledger");
        let mut rows = Vec::new();
        for _ in 0..CAP + 1 {
            admit(
                &mut rows,
                "observer/github/repo",
                "core",
                true,
                10_020_000,
                None,
            )
            .unwrap();
        }
        publish(&path, &rows).unwrap();
        let reread = read(&path, 10_019_000).unwrap();
        assert_eq!(
            reread.iter().map(|r| u128::from(r.count)).sum::<u128>(),
            (CAP + 1) as u128
        );
        assert!(reread.iter().all(|r| r.at <= 10_019_000));
    }

    #[tokio::test]
    async fn observer_refuses_corrupt_journal_and_unscoped_path_stays_available() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("ledger");
        std::fs::write(&path, b"broken").unwrap();
        assert!(
            before_send(
                path.clone(),
                Some("observer/test".into()),
                "core".into(),
                Some(CAP)
            )
            .await
            .is_err()
        );
        assert!(
            before_send(path.clone(), None, "core".into(), Some(CAP))
                .await
                .is_ok()
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"broken");
    }

    #[test]
    fn doctor_reports_writer_contention_without_waiting() {
        let _writer = state().lock().unwrap();
        let lines = doctor_lines(10_000_000);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].status, "warn");
        assert!(lines[0].message.contains("unavailable"));
    }

    #[test]
    fn crash_temporaries_are_removed_without_following_links() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("ledger");
        let old = root.path().join(".github-admission-old.tmp");
        std::fs::write(&old, b"partial").unwrap();
        let other = root.path().join("keep");
        std::fs::write(&other, b"keep").unwrap();
        let link = root.path().join(".github-admission-link.tmp");
        std::os::unix::fs::symlink(&other, &link).unwrap();
        cleanup_temporary(&path);
        assert!(!old.exists());
        assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read(other).unwrap(), b"keep");
    }

    #[test]
    fn corrupt_or_future_ledger_never_resets_to_zero() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("admission.json");
        std::fs::write(&path, b"broken").unwrap();
        assert!(read(&path, 10).is_err());
        publish(
            &path,
            &[Reservation {
                at: DAY + 11,
                caller: "observer/test".into(),
                resource: "core".into(),
                observer: true,
                count: 1,
            }],
        )
        .unwrap();
        assert!(read(&path, 10).is_err());
    }

    #[test]
    fn wrong_file_type_and_symlink_are_refused_without_blocking() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("ledger");
        std::os::unix::fs::symlink(root.path().join("target"), &path).unwrap();
        assert!(read(&path, 10).is_err());
    }
}
