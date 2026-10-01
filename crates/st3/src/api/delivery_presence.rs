//! Whether each local seat's delivery path is live and runs this daemon's st binary.
//!
//! A seat's delivery process (its native driver, or the pi-family channel) polls its mailbox every
//! second and attaches a small report: the transport, its PID, the identity of the st image it
//! executes, and for Claude the channel process that hands messages to the TUI. The daemon keeps
//! the latest report per recipient in memory. Nothing here is graph state: a restarted daemon
//! learns every live path again within a second, and a path that stopped polling is exactly the
//! fact this module exists to notice.
//!
//! A harness that reports ready while its delivery path is stale would take messages that never
//! arrive. The agent views therefore show such a seat as `waiting`, with the reason, instead of
//! `running`.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{Value, json};

/// A seat gets this long after the daemon starts before a missing poll counts as stale. Drivers
/// retry every second while the daemon restarts, and a replaced driver re-executes within two.
const STARTUP_GRACE: Duration = Duration::from_secs(20);
/// Delivery polls every second and backs off to at most 30 seconds on a failing daemon call.
const POLL_STALE_AFTER: Duration = Duration::from_secs(45);
/// The Claude channel refreshes its presence every second.
const CHANNEL_STALE_AFTER_MS: u64 = 10_000;

#[derive(Clone, Debug, Default, Deserialize)]
struct Report {
    transport: Option<String>,
    pid: Option<u32>,
    image: Option<String>,
    ready: Option<bool>,
    reason: Option<String>,
    #[serde(skip)]
    legacy: bool,
    #[serde(default)]
    channel: Option<ChannelReport>,
}

#[derive(Clone, Debug, Deserialize)]
struct ChannelReport {
    pid: Option<u32>,
    image: Option<String>,
    age_ms: Option<u64>,
}

struct Beat {
    at: Instant,
    report: Report,
}

struct Presence {
    started: Instant,
    image: Option<String>,
    beats: Mutex<HashMap<String, Beat>>,
}

fn presence() -> &'static Presence {
    static PRESENCE: OnceLock<Presence> = OnceLock::new();
    PRESENCE.get_or_init(|| Presence {
        started: Instant::now(),
        image: st_drivers::reexec::running_identity().map(|identity| identity.token()),
        beats: Mutex::new(HashMap::new()),
    })
}

/// Start the startup grace window now. The daemon calls this once when it serves its API.
pub(crate) fn start() {
    let _ = presence();
}

/// Record one mailbox poll's delivery report for `recipient`.
pub(crate) fn record(recipient: &str, report: &str) {
    let Ok(report) = serde_json::from_str::<Report>(report) else {
        return;
    };
    let presence = presence();
    if let Ok(mut beats) = presence.beats.lock() {
        beats.insert(
            recipient.to_owned(),
            Beat {
                at: Instant::now(),
                report,
            },
        );
    }
}

/// A metadata-free poll from a Unix peer proven to be this seat's native delivery process.
/// This proves liveness, not that an old executable matches the installed binary.
pub(crate) fn record_legacy(recipient: &str, transport: &str, pid: u32) {
    if let Ok(mut beats) = presence().beats.lock() {
        beats.insert(
            recipient.into(),
            Beat {
                at: Instant::now(),
                report: Report {
                    transport: Some(transport.into()),
                    pid: Some(pid),
                    legacy: true,
                    ..Report::default()
                },
            },
        );
    }
}

/// How one seat's delivery path looks from this daemon.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Assessment {
    pub(crate) state: &'static str,
    pub(crate) reason: Option<String>,
    pub(crate) polled_seconds_ago: Option<u64>,
    pub(crate) transport: Option<String>,
}

impl Assessment {
    pub(crate) fn stale(&self) -> bool {
        self.state == "stale"
    }

    pub(crate) fn to_value(&self) -> Value {
        json!({
            "state": self.state,
            "reason": self.reason,
            "polled_seconds_ago": self.polled_seconds_ago,
            "transport": self.transport,
        })
    }
}

/// Assess the delivery path of a local seat whose harness reports it can take work.
pub(crate) fn assess(recipient: &str, driver: &str) -> Assessment {
    let presence = presence();
    let beat = presence.beats.lock().ok().and_then(|beats| {
        beats
            .get(recipient)
            .map(|beat| (beat.at, beat.report.clone()))
    });
    assess_beat(
        presence.started.elapsed(),
        presence.image.as_deref(),
        beat.map(|(at, report)| (at.elapsed(), report)),
        driver,
    )
}

/// A message view may include a local poll when one exists; absent remote evidence stays absent.
pub(crate) fn known(recipient: &str) -> Option<Assessment> {
    let transport = presence()
        .beats
        .lock()
        .ok()?
        .get(recipient)?
        .report
        .transport
        .clone()?;
    Some(assess(
        recipient,
        if transport == "claude-channel" {
            "claude"
        } else {
            "other"
        },
    ))
}

fn assess_beat(
    uptime: Duration,
    daemon_image: Option<&str>,
    beat: Option<(Duration, Report)>,
    driver: &str,
) -> Assessment {
    let Some((age, report)) = beat else {
        if uptime < STARTUP_GRACE {
            return Assessment {
                state: "unknown",
                reason: Some(format!(
                    "the daemon started {}s ago and this seat has not polled its mailbox yet",
                    uptime.as_secs()
                )),
                polled_seconds_ago: None,
                transport: None,
            };
        }
        return Assessment {
            state: "stale",
            reason: Some(format!(
                "no delivery process has polled this seat's mailbox since the daemon started {}s ago",
                uptime.as_secs()
            )),
            polled_seconds_ago: None,
            transport: None,
        };
    };
    let stale = |reason: String| Assessment {
        state: "stale",
        reason: Some(reason),
        polled_seconds_ago: Some(age.as_secs()),
        transport: report.transport.clone(),
    };
    if age > POLL_STALE_AFTER {
        return stale(format!(
            "the delivery process last polled this seat's mailbox {}s ago",
            age.as_secs()
        ));
    }
    if report.legacy {
        return Assessment {
            state: "legacy",
            reason: Some(format!(
                "a legacy delivery process (pid {}) is polling; its binary and readiness are not reported",
                report.pid.unwrap_or_default()
            )),
            polled_seconds_ago: Some(age.as_secs()),
            transport: report.transport,
        };
    }
    if report.ready == Some(false) {
        return stale(report.reason.clone().unwrap_or_else(|| {
            "the channel is polling but has not received the provider's initial idle proof".into()
        }));
    }
    let current_image = |image: Option<&str>| match (image, daemon_image) {
        (Some(image), Some(daemon)) => image == daemon,
        // A daemon that cannot read its own image cannot judge anyone else's.
        (_, None) => true,
        (None, Some(_)) => false,
    };
    if !current_image(report.image.as_deref()) {
        return stale(match report.image {
            Some(_) => format!(
                "the delivery process (pid {}) still runs a replaced st binary",
                report.pid.unwrap_or_default()
            ),
            None => "the delivery process runs an st binary that predates delivery reports; restart the seat".into(),
        });
    }
    if driver == "claude" {
        let Some(channel) = report.channel.as_ref() else {
            return stale("the Claude channel has not reported that it is running".into());
        };
        if channel
            .age_ms
            .is_none_or(|age| age > CHANNEL_STALE_AFTER_MS)
        {
            return stale(format!(
                "the Claude channel (pid {}) stopped reporting {}s ago",
                channel.pid.unwrap_or_default(),
                channel.age_ms.unwrap_or_default() / 1000
            ));
        }
        if !current_image(channel.image.as_deref()) {
            return stale(format!(
                "the Claude channel (pid {}) still runs a replaced st binary",
                channel.pid.unwrap_or_default()
            ));
        }
    }
    Assessment {
        state: "current",
        reason: None,
        polled_seconds_ago: Some(age.as_secs()),
        transport: report.transport,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(image: Option<&str>, channel: Option<(Option<&str>, u64)>) -> Report {
        Report {
            transport: Some("claude-channel".into()),
            pid: Some(7),
            image: image.map(str::to_owned),
            ready: None,
            reason: None,
            legacy: false,
            channel: channel.map(|(image, age_ms)| ChannelReport {
                pid: Some(8),
                image: image.map(str::to_owned),
                age_ms: Some(age_ms),
            }),
        }
    }

    #[test]
    fn a_seat_without_a_poll_is_unknown_during_startup_and_stale_after() {
        let early = assess_beat(Duration::from_secs(3), Some("new"), None, "codex");
        assert_eq!(early.state, "unknown");
        let late = assess_beat(Duration::from_secs(60), Some("new"), None, "codex");
        assert!(late.stale(), "{late:?}");
    }

    #[test]
    fn a_live_poll_from_this_binary_is_current() {
        let assessment = assess_beat(
            Duration::from_secs(60),
            Some("new"),
            Some((
                Duration::from_secs(1),
                report(Some("new"), Some((Some("new"), 500))),
            )),
            "claude",
        );
        assert_eq!(assessment.state, "current", "{assessment:?}");
        assert_eq!(assessment.transport.as_deref(), Some("claude-channel"));
    }

    #[test]
    fn a_legacy_poll_proves_liveness_without_claiming_a_current_binary() {
        let legacy = Report {
            legacy: true,
            transport: Some("omp-channel".into()),
            pid: Some(17),
            ..Report::default()
        };
        let live = assess_beat(
            Duration::from_secs(60),
            Some("new"),
            Some((Duration::from_secs(1), legacy.clone())),
            "omp",
        );
        assert_eq!(live.state, "legacy");
        assert!(!live.stale());
        assert!(live.reason.unwrap().contains("not reported"));
        let stopped = assess_beat(
            Duration::from_secs(60),
            Some("new"),
            Some((Duration::from_secs(46), legacy)),
            "omp",
        );
        assert!(stopped.stale());
    }

    #[test]
    fn polling_before_provider_readiness_is_not_current() {
        let mut report = report(Some("new"), None);
        report.ready = Some(false);
        let starting = assess_beat(
            Duration::from_secs(60),
            Some("new"),
            Some((Duration::from_secs(1), report.clone())),
            "omp",
        );
        assert!(starting.stale());
        report.ready = Some(true);
        let ready = assess_beat(
            Duration::from_secs(60),
            Some("new"),
            Some((Duration::from_secs(1), report)),
            "omp",
        );
        assert_eq!(ready.state, "current");
    }

    #[test]
    fn a_replaced_or_silent_delivery_path_is_stale() {
        for (beat, driver, words) in [
            (
                (Duration::from_secs(1), report(Some("old"), None)),
                "codex",
                "replaced st binary",
            ),
            (
                (Duration::from_secs(1), report(None, None)),
                "codex",
                "predates delivery reports",
            ),
            (
                (Duration::from_secs(90), report(Some("new"), None)),
                "codex",
                "last polled",
            ),
            (
                (Duration::from_secs(1), report(Some("new"), None)),
                "claude",
                "has not reported",
            ),
            (
                (
                    Duration::from_secs(1),
                    report(Some("new"), Some((Some("new"), 60_000))),
                ),
                "claude",
                "stopped reporting",
            ),
            (
                (
                    Duration::from_secs(1),
                    report(Some("new"), Some((Some("old"), 100))),
                ),
                "claude",
                "channel (pid 8) still runs a replaced",
            ),
        ] {
            let assessment = assess_beat(Duration::from_secs(60), Some("new"), Some(beat), driver);
            assert!(assessment.stale(), "{assessment:?}");
            assert!(
                assessment
                    .reason
                    .as_deref()
                    .unwrap_or_default()
                    .contains(words),
                "{assessment:?} should say {words}"
            );
        }
    }

    #[test]
    fn a_report_is_recorded_per_recipient() {
        record(
            "agent/test/delivery-presence",
            r#"{"transport":"app-server","pid":1,"image":"x"}"#,
        );
        let assessment = assess("agent/test/delivery-presence", "codex");
        assert_eq!(assessment.transport.as_deref(), Some("app-server"));
        assert!(assessment.polled_seconds_ago.is_some());
    }
}
