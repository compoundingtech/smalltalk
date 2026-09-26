//! Native per-agent presence status.
//!
//! A `status` file (sibling of `agent.kdl` in the agent's dir) stores a settable state and an
//! embedded Unix-millisecond heartbeat. `unknown` is DERIVED, never written. A heartbeat older than
//! [`STATUS_STALE`] reads as `unknown`, so a crashed agent stops reading as its last set value.
//! Writes are atomic (tmp + rename), and the live session owner refreshes valid non-DND records
//! every [`STATUS_REFRESH`]. Legacy one-line records use mtime during the bounded migration. New writers
//! always emit version 1, whose freshness survives transports that do not preserve file metadata.
//! `dnd` is not refreshed after migration, so an abandoned hold ages to `unknown`.

use std::fs;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

/// A valid status heartbeat at least this old reads as `unknown`.
pub const STATUS_STALE: Duration = Duration::from_secs(15 * 60);
/// How often a live agent's status should be refreshed to stay inside the stale window — 5 min gives
/// a 3× safety margin (two missed refreshes before `unknown`).
pub const STATUS_REFRESH: Duration = Duration::from_secs(5 * 60);
/// Maximum accepted positive difference between a writer's UTC clock and the reader's clock.
pub const STATUS_FUTURE_SKEW: Duration = Duration::from_secs(60);

const RECORD_VERSION: &str = "v1";
const RECORD_PREFIX: &str = "v1 ";

/// Presence state. `Unknown` is derived from staleness and is never written to disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Offline,
    Available,
    Busy,
    Away,
    Dnd,
    Unknown,
}
impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Offline => "offline",
            State::Available => "available",
            State::Busy => "busy",
            State::Away => "away",
            State::Dnd => "dnd",
            State::Unknown => "unknown",
        }
    }

    /// Parse a *settable* state word — everything except the derived `unknown`. This is the `--set`
    /// validator: a user can never write `unknown` directly.
    pub fn parse_settable(s: &str) -> Option<State> {
        match s {
            "offline" => Some(State::Offline),
            "available" => Some(State::Available),
            "busy" => Some(State::Busy),
            "away" => Some(State::Away),
            "dnd" => Some(State::Dnd),
            _ => None,
        }
    }

    /// Parse any valid state word, including `unknown` — used when reading a file's contents.
    fn parse_any(s: &str) -> Option<State> {
        Self::parse_settable(s).or(if s == "unknown" {
            Some(State::Unknown)
        } else {
            None
        })
    }
}

/// An agent's presence file: `<agent_dir>/status`.
pub fn status_path(agent_dir: &Path) -> PathBuf {
    agent_dir.join("status")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParsedRecord {
    Legacy(State),
    Version1 { state: State, written_at_ms: u64 },
    LiteralUnknown,
    InvalidState,
    Malformed,
}

/// Read an agent's effective presence. Missing or unreadable files produce `offline`. Version 1
/// uses its embedded timestamp. A valid legacy line uses file mtime during migration. Malformed
/// versioned records fail closed to `unknown` and never fall back to mtime.
pub fn read_state(status_path: &Path) -> State {
    let (record, legacy_mtime_ms) = match read_record(status_path) {
        Ok(record) => record,
        Err(_) => return State::Offline,
    };
    read_parsed_at(record, legacy_mtime_ms, crate::message::now_ms())
}

/// Set an agent's presence to a version 1 record with the current timestamp. The write is atomic
/// (temporary sibling + rename) and creates the agent directory when needed.
pub fn set_state(status_path: &Path, state: State) -> anyhow::Result<()> {
    write_record(status_path, state, crate::message::now_ms())
}

/// Timestamp contributed by the status record to `agents --enrich`. Version 1 returns its embedded
/// writer time, clamped to the reader's current time within the allowed future skew. Legacy records
/// return mtime during migration. Invalid records and excessive future skew contribute nothing.
pub fn activity_time_ms(status_path: &Path) -> Option<f64> {
    let (record, legacy_mtime_ms) = read_record(status_path).ok()?;
    activity_time_at(record, legacy_mtime_ms, crate::message::now_ms())
        .map(|timestamp| timestamp as f64)
}

/// Outcome of a [`refresh`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshOutcome {
    /// A valid record received a new heartbeat or completed its one-time legacy upgrade.
    Refreshed,
    /// File recorded `dnd` → left untouched so an abandoned hold ages out.
    LeftDnd,
    /// File missing → wrote `available` (the sensible default for a connected agent).
    WroteDefault,
    /// File present but its contents aren't a settable state → left untouched.
    LeftCorrupt,
    /// File recorded `unknown` (which we never write) → left untouched.
    LeftUnknown,
    /// A read or write failed — best-effort; caller decides whether to log.
    Error,
}

/// Refresh the embedded heartbeat for a live agent while preserving its state. Missing writes
/// `available`. A legacy non-DND record upgrades with the current time. A legacy DND record upgrades
/// once with its old mtime. Version 1 DND, `unknown`, and malformed records remain untouched.
pub fn refresh(status_path: &Path) -> RefreshOutcome {
    let (record, legacy_mtime_ms) = match read_record(status_path) {
        Ok(record) => record,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return match write_record(status_path, State::Available, crate::message::now_ms()) {
                Ok(()) => RefreshOutcome::WroteDefault,
                Err(_) => RefreshOutcome::Error,
            };
        }
        Err(_) => return RefreshOutcome::Error,
    };
    match record {
        ParsedRecord::Version1 {
            state: State::Dnd, ..
        } => RefreshOutcome::LeftDnd,
        ParsedRecord::Version1 { state, .. } => {
            match write_record(status_path, state, crate::message::now_ms()) {
                Ok(()) => RefreshOutcome::Refreshed,
                Err(_) => RefreshOutcome::Error,
            }
        }
        ParsedRecord::Legacy(State::Dnd) => {
            let Some(written_at_ms) = legacy_mtime_ms else {
                return RefreshOutcome::LeftDnd;
            };
            match write_record(status_path, State::Dnd, written_at_ms) {
                Ok(()) => RefreshOutcome::Refreshed,
                Err(_) => RefreshOutcome::Error,
            }
        }
        ParsedRecord::Legacy(state) => {
            match write_record(status_path, state, crate::message::now_ms()) {
                Ok(()) => RefreshOutcome::Refreshed,
                Err(_) => RefreshOutcome::Error,
            }
        }
        ParsedRecord::LiteralUnknown => RefreshOutcome::LeftUnknown,
        ParsedRecord::InvalidState | ParsedRecord::Malformed => RefreshOutcome::LeftCorrupt,
    }
}

fn read_record(path: &Path) -> std::io::Result<(ParsedRecord, Option<u64>)> {
    let mut file = fs::File::open(path)?;
    let mut raw = String::new();
    file.read_to_string(&mut raw)?;
    let record = parse_record(&raw);
    let legacy_mtime_ms = matches!(record, ParsedRecord::Legacy(_))
        .then(|| file_mtime_ms(&file))
        .flatten();
    Ok((record, legacy_mtime_ms))
}

fn parse_record(raw: &str) -> ParsedRecord {
    let has_final_newline = raw.ends_with('\n');
    let body = raw.strip_suffix('\n').unwrap_or(raw);
    let mut lines = body.split('\n');
    let state = match State::parse_any(lines.next().unwrap_or("")) {
        Some(State::Unknown) => return ParsedRecord::LiteralUnknown,
        Some(state) => state,
        None => return ParsedRecord::InvalidState,
    };
    let Some(version_line) = lines.next() else {
        return ParsedRecord::Legacy(state);
    };
    if !has_final_newline || lines.next().is_some() {
        return ParsedRecord::Malformed;
    }
    let Some(timestamp) = version_line.strip_prefix(RECORD_PREFIX) else {
        return ParsedRecord::Malformed;
    };
    if timestamp.is_empty() || !timestamp.bytes().all(|byte| byte.is_ascii_digit()) {
        return ParsedRecord::Malformed;
    }
    match timestamp.parse::<u64>() {
        Ok(written_at_ms) => ParsedRecord::Version1 {
            state,
            written_at_ms,
        },
        Err(_) => ParsedRecord::Malformed,
    }
}

fn read_parsed_at(record: ParsedRecord, legacy_mtime_ms: Option<u64>, now_ms: u64) -> State {
    match record {
        ParsedRecord::Legacy(state) => legacy_mtime_ms.map_or(State::Unknown, |written_at_ms| {
            effective_state_at(state, written_at_ms, now_ms)
        }),
        ParsedRecord::Version1 {
            state,
            written_at_ms,
        } => effective_state_at(state, written_at_ms, now_ms),
        ParsedRecord::LiteralUnknown | ParsedRecord::Malformed => State::Unknown,
        ParsedRecord::InvalidState => State::Offline,
    }
}

fn effective_state_at(state: State, written_at_ms: u64, now_ms: u64) -> State {
    if written_at_ms > now_ms {
        return if written_at_ms - now_ms <= duration_ms(STATUS_FUTURE_SKEW) {
            state
        } else {
            State::Unknown
        };
    }
    if now_ms - written_at_ms >= duration_ms(STATUS_STALE) {
        State::Unknown
    } else {
        state
    }
}

fn activity_time_at(
    record: ParsedRecord,
    legacy_mtime_ms: Option<u64>,
    now_ms: u64,
) -> Option<u64> {
    let written_at_ms = match record {
        ParsedRecord::Legacy(_) => legacy_mtime_ms?,
        ParsedRecord::Version1 { written_at_ms, .. } => written_at_ms,
        ParsedRecord::LiteralUnknown | ParsedRecord::InvalidState | ParsedRecord::Malformed => {
            return None;
        }
    };
    if written_at_ms > now_ms {
        (written_at_ms - now_ms <= duration_ms(STATUS_FUTURE_SKEW)).then_some(now_ms)
    } else {
        Some(written_at_ms)
    }
}

fn file_mtime_ms(file: &fs::File) -> Option<u64> {
    file.metadata()
        .ok()?
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn write_record(path: &Path, state: State, written_at_ms: u64) -> anyhow::Result<()> {
    anyhow::ensure!(
        state != State::Unknown,
        "unknown is derived and cannot be written"
    );
    write_atomic(
        path,
        &format!("{}\n{RECORD_VERSION} {written_at_ms}\n", state.as_str()),
    )
}

/// The staging-name prefix this module hands [`crate::fsatomic`].
const TMP_PREFIX: &str = ".status";

/// The full staged-name prefix six catalog and publication walkers match, spelled ONCE here so a
/// walker can never drift from the writer. Tied to [`TMP_PREFIX`] by
/// `the_staging_prefix_is_the_one_the_catalog_walkers_match`.
pub(crate) const TMP_STAGING_PREFIX: &str = ".status.tmp-";

/// Atomic write: a staged sibling + rename, so a concurrent reader sees either the old bytes or
/// the new bytes, never a partial file.
///
/// Deliberately the lenient durability level: this record is rewritten per agent per refresh
/// tick, and a lost write reads as `unknown` rather than as a wrong state, so buying crash
/// durability with two fsyncs per tick per agent would be paying for nothing.
fn write_atomic(path: &Path, content: &str) -> anyhow::Result<()> {
    crate::fsatomic::replace(
        path,
        content.as_bytes(),
        crate::fsatomic::Staging::new(TMP_PREFIX),
        crate::fsatomic::Durability::Rename,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration as Dur, SystemTime};

    /// The staged name is a contract with six walkers that skip it by prefix, in three other
    /// modules: `catalog.rs`, `catalog_transaction.rs` and `agent_publish.rs` all match
    /// [`TMP_STAGING_PREFIX`], so a changed writer prefix cannot desynchronize them. What a const
    /// cannot catch is the prefix being renamed on BOTH sides at once — a staged status file would
    /// then still be skipped locally, but the fleet's existing records and any other reader of the
    /// old name would not. That is what this assertion is for.
    #[test]
    fn the_staging_prefix_is_the_one_the_catalog_walkers_match() {
        assert_eq!(TMP_PREFIX, ".status");
        assert_eq!(TMP_STAGING_PREFIX, format!("{TMP_PREFIX}.tmp-"));
    }

    /// [`write_atomic`]'s publication contract: the target ends up carrying the complete new
    /// bytes, no staged sibling survives, and the record is owner-only.
    ///
    /// The mode is the deliberate change of the fold onto `fsatomic` — this record used to be
    /// published at whatever an ordinary write produces (`0644` under the fleet's umask). The
    /// staging-name grammar `{prefix}.tmp-{pid}-{counter}` is unchanged and stays load-bearing:
    /// six catalog and publication walkers match `.status.tmp-` by prefix, and the grammar itself
    /// is asserted by `fsatomic`'s own test.
    #[test]
    fn a_status_write_replaces_the_target_and_leaves_no_staged_sibling() {
        use std::os::unix::fs::PermissionsExt as _;

        let tmp = tempfile::tempdir().unwrap();
        let path = status_path(tmp.path());
        write_atomic(&path, "available\n").unwrap();
        write_atomic(&path, "working\n").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "working\n");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600,
            "the status record is published owner-only"
        );

        let residue = fs::read_dir(tmp.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(".status.tmp-"))
            .collect::<Vec<_>>();
        assert!(
            residue.is_empty(),
            "staging residue left behind: {residue:?}"
        );
    }

    #[test]
    fn missing_is_offline() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(read_state(&status_path(tmp.path())), State::Offline);
    }

    #[test]
    fn set_then_read_roundtrips_each_settable_state() {
        let tmp = tempfile::tempdir().unwrap();
        let sp = status_path(tmp.path());
        for st in [
            State::Offline,
            State::Available,
            State::Busy,
            State::Away,
            State::Dnd,
        ] {
            let before = crate::message::now_ms();
            set_state(&sp, st).unwrap();
            let after = crate::message::now_ms();
            assert_eq!(read_state(&sp), st);
            let raw = fs::read_to_string(&sp).unwrap();
            let ParsedRecord::Version1 {
                state,
                written_at_ms,
            } = parse_record(&raw)
            else {
                panic!("new writer did not emit version 1: {raw:?}");
            };
            assert_eq!(state, st);
            assert!((before..=after).contains(&written_at_ms));
            assert_eq!(raw, format!("{}\nv1 {written_at_ms}\n", st.as_str()));
        }
    }

    #[test]
    fn unknown_is_not_settable() {
        assert!(State::parse_settable("unknown").is_none());
        assert!(State::parse_settable("available").is_some());
        assert!(State::parse_settable("bogus").is_none());

        let tmp = tempfile::tempdir().unwrap();
        assert!(set_state(&status_path(tmp.path()), State::Unknown).is_err());
    }

    #[test]
    fn corrupt_contents_read_as_offline() {
        let tmp = tempfile::tempdir().unwrap();
        let sp = status_path(tmp.path());
        fs::write(&sp, "garbage\n").unwrap();
        assert_eq!(read_state(&sp), State::Offline);
    }

    #[test]
    fn malformed_versioned_record_is_unknown_without_mtime_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        let sp = status_path(tmp.path());
        for raw in [
            "available\nv2 100\n",
            "available\nv1 nope\n",
            "available\nv1 100",
            "available\nv1 100\nextra\n",
            "available\n\n",
        ] {
            fs::write(&sp, raw).unwrap();
            fs::File::open(&sp)
                .unwrap()
                .set_modified(SystemTime::now())
                .unwrap();
            assert_eq!(read_state(&sp), State::Unknown, "raw: {raw:?}");
        }
    }

    #[test]
    fn literal_unknown_remains_derived_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        let sp = status_path(tmp.path());
        fs::write(&sp, "unknown\n").unwrap();
        assert_eq!(read_state(&sp), State::Unknown);
    }

    #[test]
    fn version_1_uses_embedded_time_instead_of_file_mtime() {
        let tmp = tempfile::tempdir().unwrap();
        let sp = status_path(tmp.path());
        let now_ms = crate::message::now_ms();
        write_record(&sp, State::Busy, now_ms).unwrap();
        fs::File::open(&sp)
            .unwrap()
            .set_modified(SystemTime::now() - STATUS_STALE - Dur::from_secs(60))
            .unwrap();
        assert_eq!(read_state(&sp), State::Busy);
    }

    #[test]
    fn version_1_staleness_and_future_skew_are_bounded() {
        let now_ms = 2_000_000_u64;
        let stale_ms = duration_ms(STATUS_STALE);
        let skew_ms = duration_ms(STATUS_FUTURE_SKEW);

        for (written_at_ms, expected) in [
            (now_ms - stale_ms + 1, State::Away),
            (now_ms - stale_ms, State::Unknown),
            (now_ms + skew_ms, State::Away),
            (now_ms + skew_ms + 1, State::Unknown),
        ] {
            let record = ParsedRecord::Version1 {
                state: State::Away,
                written_at_ms,
            };
            assert_eq!(read_parsed_at(record, None, now_ms), expected);
        }
    }

    #[test]
    fn legacy_record_uses_mtime_with_the_same_skew_bound() {
        let tmp = tempfile::tempdir().unwrap();
        let sp = status_path(tmp.path());
        fs::write(&sp, "busy\n").unwrap();

        fs::File::open(&sp)
            .unwrap()
            .set_modified(SystemTime::now() - STATUS_STALE - Dur::from_secs(60))
            .unwrap();
        assert_eq!(read_state(&sp), State::Unknown);

        fs::File::open(&sp)
            .unwrap()
            .set_modified(SystemTime::now() + STATUS_FUTURE_SKEW + Dur::from_secs(60))
            .unwrap();
        assert_eq!(read_state(&sp), State::Unknown);
    }

    #[test]
    fn refresh_preserves_value_and_changes_heartbeat_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let sp = status_path(tmp.path());
        write_record(&sp, State::Busy, 1).unwrap();
        assert_eq!(refresh(&sp), RefreshOutcome::Refreshed);
        assert_eq!(read_state(&sp), State::Busy);
        let ParsedRecord::Version1 {
            state,
            written_at_ms,
        } = parse_record(&fs::read_to_string(&sp).unwrap())
        else {
            panic!("refreshed record is not version 1");
        };
        assert_eq!(state, State::Busy);
        assert!(written_at_ms > 1);
    }

    #[test]
    fn refresh_missing_writes_available_default() {
        let tmp = tempfile::tempdir().unwrap();
        let sp = status_path(tmp.path());
        assert_eq!(refresh(&sp), RefreshOutcome::WroteDefault);
        assert_eq!(read_state(&sp), State::Available);
        assert!(matches!(
            parse_record(&fs::read_to_string(&sp).unwrap()),
            ParsedRecord::Version1 {
                state: State::Available,
                ..
            }
        ));
    }

    #[test]
    fn refresh_leaves_dnd_to_age_out() {
        let tmp = tempfile::tempdir().unwrap();
        let sp = status_path(tmp.path());
        set_state(&sp, State::Dnd).unwrap();
        let before = fs::metadata(&sp).unwrap().modified().unwrap();
        std::thread::sleep(Dur::from_millis(5));

        assert_eq!(refresh(&sp), RefreshOutcome::LeftDnd);
        assert_eq!(fs::metadata(&sp).unwrap().modified().unwrap(), before);
        assert_eq!(read_state(&sp), State::Dnd);
    }

    #[test]
    fn refresh_upgrades_legacy_non_dnd_with_current_heartbeat() {
        let tmp = tempfile::tempdir().unwrap();
        let sp = status_path(tmp.path());
        fs::write(&sp, "away\n").unwrap();

        let before = crate::message::now_ms();
        assert_eq!(refresh(&sp), RefreshOutcome::Refreshed);
        let after = crate::message::now_ms();
        let ParsedRecord::Version1 {
            state,
            written_at_ms,
        } = parse_record(&fs::read_to_string(&sp).unwrap())
        else {
            panic!("legacy record was not upgraded");
        };
        assert_eq!(state, State::Away);
        assert!((before..=after).contains(&written_at_ms));
    }

    #[test]
    fn refresh_upgrades_legacy_dnd_without_renewing_the_hold() {
        let tmp = tempfile::tempdir().unwrap();
        let sp = status_path(tmp.path());
        fs::write(&sp, "dnd\n").unwrap();
        fs::File::open(&sp)
            .unwrap()
            .set_modified(SystemTime::now() - STATUS_STALE - Dur::from_secs(60))
            .unwrap();
        let legacy_timestamp = file_mtime_ms(&fs::File::open(&sp).unwrap()).unwrap();

        assert_eq!(refresh(&sp), RefreshOutcome::Refreshed);
        let raw = fs::read_to_string(&sp).unwrap();
        assert_eq!(
            parse_record(&raw),
            ParsedRecord::Version1 {
                state: State::Dnd,
                written_at_ms: legacy_timestamp,
            }
        );
        assert_eq!(read_state(&sp), State::Unknown);
        assert_eq!(refresh(&sp), RefreshOutcome::LeftDnd);
        assert_eq!(fs::read_to_string(&sp).unwrap(), raw);
    }

    #[test]
    fn refresh_leaves_corrupt_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let sp = status_path(tmp.path());
        for raw in ["garbage\n", "available\nv1 nope\n"] {
            fs::write(&sp, raw).unwrap();
            assert_eq!(refresh(&sp), RefreshOutcome::LeftCorrupt);
            assert_eq!(fs::read_to_string(&sp).unwrap(), raw);
        }
    }

    #[test]
    fn activity_uses_origin_time_and_clamps_only_allowed_future_skew() {
        let now_ms = 2_000_000_u64;
        let skew_ms = duration_ms(STATUS_FUTURE_SKEW);

        let record = ParsedRecord::Version1 {
            state: State::Available,
            written_at_ms: 1234,
        };
        assert_eq!(activity_time_at(record, None, now_ms), Some(1234));
        assert_eq!(
            activity_time_at(ParsedRecord::Legacy(State::Busy), Some(1234), now_ms),
            Some(1234)
        );

        let bounded_future = ParsedRecord::Version1 {
            state: State::Available,
            written_at_ms: now_ms + skew_ms,
        };
        assert_eq!(activity_time_at(bounded_future, None, now_ms), Some(now_ms));

        let excessive_future = ParsedRecord::Version1 {
            state: State::Available,
            written_at_ms: now_ms + skew_ms + 1,
        };
        assert_eq!(activity_time_at(excessive_future, None, now_ms), None);
        assert_eq!(
            activity_time_at(ParsedRecord::Malformed, None, now_ms),
            None
        );
    }
}
