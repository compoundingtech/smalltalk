//! How big this member's store is, and how much it grows a day: the database target in
//! `slo/targets.toml`, and the physical growth that disk-filling conditions read. The daemon
//! samples the store hourly and keeps two days of samples in `meta`, so the daily growth survives
//! restarts. A sample reads three pragmas and the WAL file's length. It is the one sampler: the
//! newest sample is cached for every reader (`st3::slo::database_size`).

use super::*;

/// The `meta` key holding recent samples:
/// `[[unix_ms, live_bytes, file_bytes, wal_bytes, main_file_bytes], ...]`, oldest first. Samples
/// written before the files were measured have three elements.
const SAMPLES: &str = "database_size_samples";
/// Samples older than this are forgotten.
const KEEP_MS: u128 = 49 * 60 * 60 * 1000;
/// At most this many samples are kept, whatever the restarts.
const MAX_SAMPLES: usize = 128;
const DAY_MS: u128 = 24 * 60 * 60 * 1000;
/// The shortest span a daily growth is extrapolated from.
const MIN_SPAN_MS: u128 = 60 * 60 * 1000;
/// How far from exactly a day ago a physical baseline may be.
const BASELINE_TOLERANCE_MS: u128 = 30 * 60 * 1000;

/// One stored sample: when, the pages in use, the pages in all, and the lengths of the main file
/// and its WAL when they were measured.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Sample {
    at: u128,
    live: u64,
    file: u64,
    wal: Option<u64>,
    main: Option<u64>,
}

impl Sample {
    fn from_value(value: &Value) -> Option<Self> {
        let item = |index: usize| value.get(index).and_then(Value::as_u64);
        Some(Sample {
            at: u128::from(item(0)?),
            live: item(1)?,
            file: item(2)?,
            wal: item(3),
            main: item(4),
        })
    }

    fn to_value(self) -> Value {
        let mut row = vec![json!(self.at as u64), json!(self.live), json!(self.file)];
        if let (Some(wal), Some(main)) = (self.wal, self.main) {
            row.extend([json!(wal), json!(main)]);
        }
        Value::Array(row)
    }

    /// The main file's length and its WAL's: what the store takes on disk.
    fn physical(self) -> Option<u64> {
        Some(self.main? + self.wal?)
    }
}

/// One measurement of the store.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DatabaseSize {
    pub measured_at_unix_ms: u128,
    /// The store's pages, including free ones and those committed to the WAL but not yet copied
    /// into the main file.
    pub file_bytes: u64,
    /// The pages in use: the file less its free pages, which SQLite reuses before growing.
    pub live_bytes: u64,
    /// How much `live_bytes` grew a day, from the sample nearest a day ago (or the oldest, when
    /// the store has been sampled for less than a day but at least an hour). Negative when
    /// checkpoints freed more than was written.
    pub growth_bytes_per_day: Option<i64>,
    /// The span the growth was measured over.
    pub growth_span_ms: Option<u128>,
    /// The WAL file's length beside the store.
    pub wal_bytes: u64,
    /// The main file's length. Pages committed to the WAL and not yet copied in are not in it.
    pub main_file_bytes: u64,
    /// What the store takes on disk: the main file's length and the WAL's. Page counts include
    /// pages still in the WAL, so they are not added to it.
    pub physical_bytes: u64,
    /// How much `physical_bytes` grew since the sample nearest exactly a day ago, within half an
    /// hour of it, scaled to a day. Never extrapolated: none until that baseline exists.
    pub physical_growth_bytes_per_day: Option<i64>,
    /// The span the physical growth was measured over, within half an hour of a day.
    pub physical_growth_span_ms: Option<u128>,
}

/// The daily physical growth at `now` for `physical` bytes, from the sample with a measured WAL
/// nearest exactly a day ago and within half an hour of it, and the span it was measured over.
fn physical_growth(samples: &[Sample], now: u128, physical: u64) -> Option<(i64, u128)> {
    let day_ago = now.checked_sub(DAY_MS)?;
    let (span, then) = samples
        .iter()
        .filter_map(|sample| Some((sample.at, sample.physical()?)))
        .filter(|(at, _)| at.abs_diff(day_ago) <= BASELINE_TOLERANCE_MS)
        .min_by_key(|(at, _)| (at.abs_diff(day_ago), *at))
        .map(|(at, then)| (now - at, then))?;
    let delta = i128::from(physical) - i128::from(then);
    Some((i64::try_from(delta * DAY_MS as i128 / span as i128).ok()?, span))
}

/// The daily growth `samples` show at `now` for `live` bytes, and the span it was measured over.
pub(crate) fn growth(samples: &[(u128, u64, u64)], now: u128, live: u64) -> Option<(i64, u128)> {
    let day_ago = now.saturating_sub(DAY_MS);
    // The newest sample at least a day old, or else the oldest sample.
    let (at, then, _) = samples
        .iter()
        .rev()
        .find(|(at, _, _)| *at <= day_ago)
        .or_else(|| samples.first())?;
    let span = now.checked_sub(*at)?;
    if span < MIN_SPAN_MS {
        return None;
    }
    let delta = i128::from(live) - i128::from(*then);
    let per_day = delta * DAY_MS as i128 / span as i128;
    Some((i64::try_from(per_day).ok()?, span))
}

impl Store {
    /// Measure the store and remember the sample. The daemon calls it hourly.
    pub fn record_database_size(&self, now: u128) -> Result<DatabaseSize> {
        let (page_size, pages, free) = {
            let connection = self.readers.get();
            let pragma = |name: &str| -> Result<u64> {
                Ok(connection.query_row(&format!("PRAGMA {name}"), [], |row| row.get(0))?)
            };
            (pragma("page_size")?, pragma("page_count")?, pragma("freelist_count")?)
        };
        let file_bytes = page_size * pages;
        let live_bytes = page_size * pages.saturating_sub(free);
        // A store without a WAL file beside it (in memory, or just checkpointed away) has none.
        let mut wal = self.path.clone().into_os_string();
        wal.push("-wal");
        let length = |path: &std::ffi::OsStr| std::fs::metadata(path).map_or(0, |metadata| metadata.len());
        let wal_bytes = length(&wal);
        let main_file_bytes = length(self.path.as_os_str());
        let physical_bytes = main_file_bytes + wal_bytes;
        let mut connection = self.connection.write();
        let transaction = connection.transaction()?;
        let mut samples: Vec<Sample> = transaction
            .query_row("SELECT value FROM meta WHERE key=?1", [SAMPLES], |row| {
                row.get::<_, String>(0)
            })
            .optional()?
            .and_then(|text| serde_json::from_str::<Vec<Value>>(&text).ok())
            .unwrap_or_default()
            .iter()
            .filter_map(Sample::from_value)
            .collect();
        // A sample from the future means the clock went back: it is dropped, never a baseline.
        samples.retain(|sample| sample.at <= now && now - sample.at <= KEEP_MS);
        let triples = samples.iter().map(|sample| (sample.at, sample.live, sample.file)).collect::<Vec<_>>();
        let growth = growth(&triples, now, live_bytes);
        let physical = physical_growth(&samples, now, physical_bytes);
        samples.push(Sample {
            at: now,
            live: live_bytes,
            file: file_bytes,
            wal: Some(wal_bytes),
            main: Some(main_file_bytes),
        });
        let excess = samples.len().saturating_sub(MAX_SAMPLES);
        samples.drain(..excess);
        let stored = Value::Array(samples.iter().map(|sample| sample.to_value()).collect());
        transaction.execute(
            "INSERT INTO meta(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![SAMPLES, stored.to_string()],
        )?;
        transaction.commit()?;
        Ok(DatabaseSize {
            measured_at_unix_ms: now,
            file_bytes,
            live_bytes,
            growth_bytes_per_day: growth.map(|(per_day, _)| per_day),
            growth_span_ms: growth.map(|(_, span)| span),
            wal_bytes,
            main_file_bytes,
            physical_bytes,
            physical_growth_bytes_per_day: physical.map(|(per_day, _)| per_day),
            physical_growth_span_ms: physical.map(|(_, span)| span),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: u128 = 60 * 60 * 1000;

    #[test]
    fn growth_is_measured_from_a_day_ago_or_extrapolated_from_an_hour() {
        let now = 100 * DAY_MS;
        assert_eq!(growth(&[], now, 10), None);
        // Less than an hour of history says nothing.
        assert_eq!(growth(&[(now - HOUR / 2, 0, 0)], now, 10), None);
        // Two hours, 100 bytes: 1,200 a day.
        assert_eq!(growth(&[(now - 2 * HOUR, 0, 0)], now, 100), Some((1_200, 2 * HOUR)));
        // With samples on both sides of a day ago, the newest at least a day old.
        let samples = [(now - 30 * HOUR, 0, 0), (now - 25 * HOUR, 500, 0), (now - 3 * HOUR, 900, 0)];
        assert_eq!(growth(&samples, now, 1_500), Some((960, 25 * HOUR)));
        // A trim that frees more than was written is negative growth.
        assert_eq!(growth(&[(now - DAY_MS, 1_000, 0)], now, 400), Some((-600, DAY_MS)));
    }

    fn sample(at: u128, physical: Option<u64>) -> Sample {
        Sample { at, live: 0, file: 0, wal: physical.map(|_| 0), main: physical }
    }

    #[test]
    fn physical_growth_needs_a_baseline_within_half_an_hour_of_a_day_ago() {
        let now = 100 * DAY_MS;
        // No extrapolation: twenty hours of history say nothing.
        assert_eq!(physical_growth(&[sample(now - 20 * HOUR, Some(0))], now, 100), None);
        // A day and twenty minutes ago is close enough, scaled to a day.
        let span = DAY_MS + 20 * 60 * 1000;
        let grown = physical_growth(&[sample(now - span, Some(0))], now, 1_000).unwrap();
        assert_eq!(grown, (i64::try_from(1_000 * DAY_MS / span).unwrap(), span));
        // A day and forty minutes is not.
        assert_eq!(physical_growth(&[sample(now - DAY_MS - 40 * 60 * 1000, Some(0))], now, 1_000), None);
        // The nearest of several, and never one without a measured WAL.
        let samples = [
            sample(now - DAY_MS - 25 * 60 * 1000, Some(100)),
            sample(now - DAY_MS + 5 * 60 * 1000, Some(400)),
            sample(now - DAY_MS, None),
        ];
        assert_eq!(physical_growth(&samples, now, 400).map(|(per_day, _)| per_day), Some(0));
    }

    #[test]
    fn a_sample_from_before_the_wal_was_measured_still_reads() {
        let old = Sample::from_value(&json!([5, 1, 2])).unwrap();
        assert_eq!(old, Sample { at: 5, live: 1, file: 2, wal: None, main: None });
        assert_eq!(old.physical(), None, "never a physical baseline");
        let measured = Sample { at: 5, live: 1, file: 9, wal: Some(3), main: Some(4) };
        assert_eq!(Sample::from_value(&measured.to_value()), Some(measured));
        // The main file and the WAL, not the page count, which includes the WAL's pages.
        assert_eq!(measured.physical(), Some(7));
    }

    #[test]
    fn a_clock_that_went_back_drops_the_future_samples() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(&directory.path().join("claims.sqlite3"), "alder").unwrap();
        let start = 100 * DAY_MS;
        store.record_database_size(start + DAY_MS).unwrap();
        // The clock goes back a day: the later sample is no baseline and is forgotten.
        let back = store.record_database_size(start).unwrap();
        assert_eq!((back.growth_bytes_per_day, back.physical_growth_bytes_per_day), (None, None));
        let next = store.record_database_size(start + DAY_MS).unwrap();
        assert!(next.physical_growth_bytes_per_day.is_some(), "the earlier sample is a day old");
        assert_eq!(next.physical_bytes, next.main_file_bytes + next.wal_bytes);
        let on_disk = |suffix: &str| {
            std::fs::metadata(directory.path().join(format!("claims.sqlite3{suffix}"))).map_or(0, |m| m.len())
        };
        assert_eq!((next.main_file_bytes, next.wal_bytes), (on_disk(""), on_disk("-wal")));
    }

    #[test]
    fn samples_survive_reopening_and_age_out() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("claims.sqlite3");
        let start = 100 * DAY_MS;
        {
            let store = Store::open(&path, "alder").unwrap();
            let first = store.record_database_size(start).unwrap();
            assert!(first.live_bytes > 0 && first.file_bytes >= first.live_bytes);
            assert_eq!(first.growth_bytes_per_day, None);
        }
        let store = Store::open(&path, "alder").unwrap();
        let later = store.record_database_size(start + 2 * HOUR).unwrap();
        assert_eq!(later.growth_span_ms, Some(2 * HOUR));
        assert!(later.growth_bytes_per_day.is_some());
        // Three days later the old samples are gone, so there is no growth to report yet.
        let much_later = store.record_database_size(start + 3 * DAY_MS).unwrap();
        assert_eq!(much_later.growth_bytes_per_day, None);
    }
}
