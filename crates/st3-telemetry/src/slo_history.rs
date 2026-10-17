//! A local observation log of what is left of each objective and budget.
//!
//! The live windows are process memory, and the collector holds their long history when one is
//! configured. A node without a collector still has to chart `st slo`, so once a minute the daemon
//! appends one line to `<state>/slo-history/YYYY-MM-DD.jsonl` (UTC day): the time and the
//! remaining percent of every row of `st slo ls`. Nothing reads a line back except a chart, and
//! the log never enters the graph or replicates. A file older than seven days, the same horizon as
//! `[observations] retention`, is deleted when the next line is appended. A restart loses nothing
//! but the minutes the daemon was down.
//!
//! A line is `{"t": UNIX_MS, "r": {"ID": PERCENT, ...}}`. A row with nothing measured is left out
//! of the line, so a gap in a chart means "no measurement", not a spent budget.

use std::collections::BTreeMap;
use std::io::{BufRead as _, Write as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};

const DAY_MS: u64 = 86_400_000;
/// The longest period a chart can ask for.
pub const RETENTION_MS: u64 = 7 * DAY_MS;
const INTERVAL: Duration = Duration::from_secs(60);

pub fn dir(state_dir: &Path) -> PathBuf {
    state_dir.join("slo-history")
}

fn file(dir: &Path, day: u64) -> PathBuf {
    let date = chrono::DateTime::<chrono::Utc>::from_timestamp_millis((day * DAY_MS) as i64)
        .map_or_else(|| "invalid".to_owned(), |at| at.format("%Y-%m-%d").to_string());
    dir.join(format!("{date}.jsonl"))
}

/// Append one line for `rows` (the rows `objectives::rows` returns) at `now_unix_ms`, and delete
/// the days that have aged out.
pub fn append(dir: &Path, now_unix_ms: u64, rows: &[Value]) -> std::io::Result<()> {
    let remaining = rows
        .iter()
        .filter_map(|row| Some((row["id"].as_str()?, row["remaining_percent"].as_f64()?)))
        .collect::<BTreeMap<_, _>>();
    if remaining.is_empty() {
        return Ok(());
    }
    std::fs::create_dir_all(dir)?;
    let oldest = (now_unix_ms.saturating_sub(RETENTION_MS)) / DAY_MS;
    for entry in std::fs::read_dir(dir)?.flatten() {
        let path = entry.path();
        let expired = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .and_then(|stem| chrono::NaiveDate::parse_from_str(stem, "%Y-%m-%d").ok())
            .and_then(|date| date.and_hms_opt(0, 0, 0))
            .is_some_and(|date| (date.and_utc().timestamp_millis() as u64) / DAY_MS < oldest);
        if expired && path.extension().is_some_and(|extension| extension == "jsonl") {
            std::fs::remove_file(path)?;
        }
    }
    let mut line = serde_json::to_vec(&json!({"t": now_unix_ms, "r": remaining}))?;
    line.push(b'\n');
    let mut log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(file(dir, now_unix_ms / DAY_MS))?;
    log.write_all(&line)
}

/// Every reading of `id` from `since_ms` to `until_ms`, oldest first. A torn or foreign line is
/// skipped.
pub fn read(dir: &Path, id: &str, since_ms: u64, until_ms: u64) -> std::io::Result<Vec<(u64, f64)>> {
    let mut points = Vec::new();
    for day in since_ms / DAY_MS..=until_ms / DAY_MS {
        let log = match std::fs::File::open(file(dir, day)) {
            Ok(log) => log,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        for line in std::io::BufReader::new(log).lines() {
            let Ok(line) = line else { break };
            let Ok(line) = serde_json::from_str::<Value>(&line) else { continue };
            if let (Some(at), Some(percent)) = (line["t"].as_u64(), line["r"][id].as_f64())
                && (since_ms..=until_ms).contains(&at)
            {
                points.push((at, percent));
            }
        }
    }
    Ok(points)
}

/// `points` in `count` equal steps from `since_ms` to `until_ms`. A step holds its lowest
/// reading, because the worst moment is what a budget chart is for; a step with no reading is
/// `None`.
pub fn buckets(points: &[(u64, f64)], since_ms: u64, until_ms: u64, count: usize) -> Vec<Option<f64>> {
    let mut steps = vec![None::<f64>; count];
    let span = until_ms.saturating_sub(since_ms).max(1) as u128;
    for (at, percent) in points {
        let step = ((at.saturating_sub(since_ms)) as u128 * count as u128 / span).min(count as u128 - 1) as usize;
        steps[step] = Some(steps[step].map_or(*percent, |lowest| lowest.min(*percent)));
    }
    steps
}

/// Sample `rows` into the log once a minute until the process stops. A failed append is reported
/// once and the next minute tries again. Blocking work is charged to `slo.history` through
/// `smallclaims::task_cpu`.
pub async fn run<F>(dir: PathBuf, rows: F)
where
    F: Fn() -> anyhow::Result<Vec<Value>> + Send + Sync + Clone + 'static,
{
    let mut failing = false;
    loop {
        tokio::time::sleep(INTERVAL).await;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_millis() as u64);
        let (log, rows) = (dir.clone(), rows.clone());
        let result = tokio::task::spawn_blocking(move || smallclaims::task_cpu::blocking("slo.history", || -> anyhow::Result<()> {
            Ok(append(&log, now, &rows()?)?)
        }))
        .await;
        match result {
            Ok(Ok(())) => failing = false,
            Ok(Err(error)) if !failing => {
                failing = true;
                eprintln!("st3: recording the SLO history failed: {error:#}");
            }
            Err(error) if !failing => {
                failing = true;
                eprintln!("st3: recording the SLO history failed: {error}");
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(cpu: Option<f64>, week: f64) -> Vec<Value> {
        vec![
            json!({"id": "cpu", "remaining_percent": cpu}),
            json!({"id": "ada/claude/weekly", "remaining_percent": week}),
        ]
    }

    #[test]
    fn a_reading_is_appended_per_minute_and_read_back_by_id() {
        let root = tempfile::tempdir().unwrap();
        let day = 20_000 * DAY_MS;
        append(root.path(), day + 60_000, &rows(Some(75.0), 60.0)).unwrap();
        append(root.path(), day + 120_000, &rows(None, 59.0)).unwrap();
        append(root.path(), day + DAY_MS + 1_000, &rows(Some(70.0), 58.0)).unwrap();
        let cpu = read(root.path(), "cpu", day, day + 2 * DAY_MS).unwrap();
        assert_eq!(cpu, vec![(day + 60_000, 75.0), (day + DAY_MS + 1_000, 70.0)], "an unmeasured minute is a gap");
        let week = read(root.path(), "ada/claude/weekly", day + 100_000, day + 2 * DAY_MS).unwrap();
        assert_eq!(week.len(), 2, "the period's edges are inclusive and exclude earlier readings");
        assert!(read(root.path(), "nope", day, day + DAY_MS).unwrap().is_empty());
        assert!(read(&root.path().join("missing"), "cpu", day, day + DAY_MS).unwrap().is_empty());
    }

    #[test]
    fn a_line_with_nothing_measured_writes_nothing_and_a_torn_line_is_skipped() {
        let root = tempfile::tempdir().unwrap();
        let day = 20_000 * DAY_MS;
        append(root.path(), day, &rows(None, 0.0)[..1]).unwrap();
        assert!(!root.path().exists() || std::fs::read_dir(root.path()).unwrap().next().is_none());
        append(root.path(), day + 1, &rows(Some(50.0), 50.0)).unwrap();
        let path = file(root.path(), 20_000);
        let mut log = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        log.write_all(b"{\"t\": 5, \"r\": {\"cp").unwrap();
        assert_eq!(read(root.path(), "cpu", day, day + DAY_MS).unwrap(), vec![(day + 1, 50.0)]);
    }

    #[test]
    fn a_day_older_than_the_retention_is_deleted_when_the_next_line_is_appended() {
        let root = tempfile::tempdir().unwrap();
        let day = 20_000 * DAY_MS;
        append(root.path(), day, &rows(Some(1.0), 1.0)).unwrap();
        append(root.path(), day + 6 * DAY_MS, &rows(Some(2.0), 2.0)).unwrap();
        assert!(file(root.path(), 20_000).exists(), "six days old is kept");
        std::fs::write(root.path().join("notes.txt"), "not a log").unwrap();
        append(root.path(), day + 9 * DAY_MS, &rows(Some(3.0), 3.0)).unwrap();
        assert!(!file(root.path(), 20_000).exists());
        assert!(file(root.path(), 20_006).exists(), "three days old is kept");
        assert!(file(root.path(), 20_009).exists());
        assert!(root.path().join("notes.txt").exists(), "only dated logs are deleted");
    }

    #[test]
    fn buckets_keep_the_lowest_reading_and_leave_gaps() {
        let points = [(0, 90.0), (10, 40.0), (30, 80.0), (95, 30.0)];
        let steps = buckets(&points, 0, 100, 4);
        assert_eq!(steps, vec![Some(40.0), Some(80.0), None, Some(30.0)]);
        assert_eq!(buckets(&[], 0, 100, 3), vec![None, None, None]);
        // A reading at the very end belongs to the last step.
        assert_eq!(buckets(&[(100, 5.0)], 0, 100, 2), vec![None, Some(5.0)]);
    }
}
