//! How big this member's store is, and how much it grows a day: the database target in
//! `slo/targets.toml`. The daemon samples the store's pages hourly and keeps two days of samples
//! in `meta`, so the daily growth survives restarts. Reading the size reads three pragmas.

use super::*;

/// The `meta` key holding recent samples: `[[unix_ms, live_bytes, file_bytes], ...]`, oldest first.
const SAMPLES: &str = "database_size_samples";
/// Samples older than this are forgotten.
const KEEP_MS: u128 = 49 * 60 * 60 * 1000;
/// At most this many samples are kept, whatever the restarts.
const MAX_SAMPLES: usize = 128;
const DAY_MS: u128 = 24 * 60 * 60 * 1000;
/// The shortest span a daily growth is extrapolated from.
const MIN_SPAN_MS: u128 = 60 * 60 * 1000;

/// One measurement of the store.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DatabaseSize {
    pub measured_at_unix_ms: u128,
    /// The file's pages, including free ones.
    pub file_bytes: u64,
    /// The pages in use: the file less its free pages, which SQLite reuses before growing.
    pub live_bytes: u64,
    /// How much `live_bytes` grew a day, from the sample nearest a day ago (or the oldest, when
    /// the store has been sampled for less than a day but at least an hour). Negative when
    /// checkpoints freed more than was written.
    pub growth_bytes_per_day: Option<i64>,
    /// The span the growth was measured over.
    pub growth_span_ms: Option<u128>,
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
        let mut connection = self.connection.write();
        let transaction = connection.transaction()?;
        let mut samples: Vec<(u128, u64, u64)> = transaction
            .query_row("SELECT value FROM meta WHERE key=?1", [SAMPLES], |row| {
                row.get::<_, String>(0)
            })
            .optional()?
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        samples.retain(|(at, _, _)| *at <= now && now - *at <= KEEP_MS);
        let growth = growth(&samples, now, live_bytes);
        samples.push((now, live_bytes, file_bytes));
        let excess = samples.len().saturating_sub(MAX_SAMPLES);
        samples.drain(..excess);
        transaction.execute(
            "INSERT INTO meta(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![SAMPLES, serde_json::to_string(&samples)?],
        )?;
        transaction.commit()?;
        Ok(DatabaseSize {
            measured_at_unix_ms: now,
            file_bytes,
            live_bytes,
            growth_bytes_per_day: growth.map(|(per_day, _)| per_day),
            growth_span_ms: growth.map(|(_, span)| span),
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
