//! Whether each local seat's delivery path is live and runs this daemon's st binary.
//!
//! A seat's delivery process (its native driver, or the pi-family channel) polls its mailbox every
//! second and attaches a small report: the transport, its PID, the identity of the st image it
//! executes, the installed binary it follows, and for Claude the channel process that hands
//! messages to the TUI. The daemon keeps the latest report per recipient in memory. Nothing here
//! is graph state: a restarted daemon learns every live path again within a second, and a path
//! that stopped polling is exactly the fact this module exists to notice.
//!
//! A harness that reports ready while its delivery path is stale would take messages that never
//! arrive. The agent views therefore show such a seat as `waiting`, with the reason, instead of
//! `running`. A ready path that still runs a replaced st binary does deliver, with its old code,
//! so it is `outdated` rather than stale; its reason says whether it will follow the daemon's
//! binary or needs a seat restart.

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
    /// The installed binary the process re-executes into once that file changes.
    follows: Option<String>,
    /// The image `follows` names now, read by the daemon when it assesses the report.
    #[serde(skip)]
    follows_image: Option<String>,
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
    fence: Option<crate::mailbox::Fence>,
}

struct Presence {
    started: Instant,
    image: Option<String>,
    beats: Mutex<HashMap<String, Beat>>,
    monitors: Mutex<HashMap<String, Beat>>,
}

fn presence() -> &'static Presence {
    static PRESENCE: OnceLock<Presence> = OnceLock::new();
    PRESENCE.get_or_init(|| Presence {
        started: Instant::now(),
        image: st_drivers::reexec::running_identity().map(|identity| identity.token()),
        beats: Mutex::new(HashMap::new()),
        monitors: Mutex::new(HashMap::new()),
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
                fence: None,
            },
        );
    }
}

/// Title updates cannot establish delivery readiness. They can report the outer driver's
/// attachment check, including a missing plugin for which no delivery process exists.
pub(super) fn record_fenced(fence: &crate::mailbox::Fence, raw: &str) -> bool {
    let Ok(report) = serde_json::from_str::<Report>(raw) else {
        return false;
    };
    let target = if fence.component == "delivery" {
        if report.transport.as_deref() != Some("claude-channel")
            && let Ok(mut monitors) = presence().monitors.lock()
        {
            monitors.remove(&fence.subject);
        }
        &presence().beats
    } else if report.transport.as_deref() == Some("claude-channel") {
        &presence().monitors
    } else {
        return false;
    };
    if let Ok(mut beats) = target.lock() {
        beats.insert(
            fence.subject.clone(),
            Beat {
                at: Instant::now(),
                report,
                fence: Some(fence.clone()),
            },
        );
        return true;
    }
    false
}

pub(super) fn attachment(recipient: &str, incarnation: &str) -> Option<crate::mailbox::Fence> {
    let beats = presence().beats.lock().ok()?;
    let beat = beats.get(recipient)?;
    let fence = beat.fence.as_ref()?;
    (fence.incarnation == incarnation
        && beat.at.elapsed() <= Duration::from_millis(CHANNEL_STALE_AFTER_MS)
        && beat.report.transport.as_deref() == Some("claude-channel")
        && beat.report.ready == Some(true)
        && beat.report.channel.as_ref().is_some_and(|channel|
            channel.age_ms.is_some_and(|age| age <= CHANNEL_STALE_AFTER_MS)))
    .then(|| fence.clone())
}

/// A metadata-free poll from a Unix peer proven to be this seat's native delivery process.
/// This proves liveness, not that an old executable matches the installed binary.
pub(crate) fn record_legacy(recipient: &str, transport: &str, pid: u32) {
    // A provider launched by an older driver has no title-side attachment monitor.
    if let Ok(mut monitors) = presence().monitors.lock() {
        monitors.remove(recipient);
    }
    if let Ok(mut beats) = presence().beats.lock() {
        beats.insert(
            recipient.into(),
            Beat {
                at: Instant::now(),
                fence: None,
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

/// Assess the delivery path of a local native seat, independently of harness readiness.
pub(crate) fn assess(recipient: &str, driver: &str) -> Assessment {
    let presence = presence();
    let mut beat = presence.beats.lock().ok().and_then(|beats| {
        beats
            .get(recipient)
            .map(|beat| (beat.at, beat.report.clone()))
    });
    if driver == "claude"
        && let Ok(monitors) = presence.monitors.lock()
        && let Some(monitor) = monitors.get(recipient)
        && monitor.report.ready == Some(false)
    {
        beat = Some((monitor.at, monitor.report.clone()));
    }
    assess_beat(
        presence.started.elapsed(),
        presence.image.as_deref(),
        beat.map(|(at, mut report)| {
            report.follows_image = report.follows.as_deref().and_then(|path| {
                st_drivers::reexec::ImageIdentity::of(std::path::Path::new(path))
                    .ok()
                    .map(|identity| identity.token())
            });
            (at.elapsed(), report)
        }),
        driver,
    )
}

/// A message view may include a local poll when one exists; absent remote evidence stays absent.
pub(crate) fn known(recipient: &str) -> Option<Assessment> {
    let transport = presence()
        .monitors
        .lock()
        .ok()
        .and_then(|beats| {
            beats
                .get(recipient)
                .and_then(|beat| beat.report.transport.clone())
        })
        .or_else(|| {
            presence().beats.lock().ok().and_then(|beats| {
                beats
                    .get(recipient)
                    .and_then(|beat| beat.report.transport.clone())
            })
        })?;
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
    // A ready path on a replaced binary still delivers, with its old code. It catches up on its
    // own only when the file it follows is now the daemon's image.
    let replaced = |process: &str, pid: Option<u32>| {
        let pid = pid.unwrap_or_default();
        match report.follows.as_deref() {
            Some(_) if current_image(report.follows_image.as_deref()) => format!(
                "the {process} (pid {pid}) still runs a replaced st binary and is switching to the daemon's"
            ),
            Some(follows) => format!(
                "the {process} (pid {pid}) runs a replaced st binary and follows {follows}, which is not the daemon's; restart the seat to update it"
            ),
            None => format!(
                "the {process} (pid {pid}) runs a replaced st binary that predates following the daemon's; restart the seat to update it"
            ),
        }
    };
    let mut outdated = None;
    if !current_image(report.image.as_deref()) {
        if report.image.is_none() {
            return stale(
                "the delivery process runs an st binary that predates delivery reports; restart the seat".into(),
            );
        }
        outdated = Some(replaced("delivery process", report.pid));
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
        if outdated.is_none() && !current_image(channel.image.as_deref()) {
            outdated = Some(replaced("Claude channel", channel.pid));
        }
    }
    Assessment {
        state: if outdated.is_some() {
            "outdated"
        } else {
            "current"
        },
        reason: outdated,
        polled_seconds_ago: Some(age.as_secs()),
        transport: report.transport,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAEMON: Option<&str> = Some("new");

    #[test]
    fn malformed_report_does_not_replace_a_beat_or_prove_recording() {
        let recipient = "agent/malformed-report-control";
        let mut fence = crate::mailbox::Fence::new(recipient, "current", "delivery");
        fence.epoch = 1;
        assert!(record_fenced(
            &fence,
            &json!({"transport":"omp-channel", "ready":false}).to_string()
        ));
        let mut malformed = fence.clone();
        malformed.epoch = 2;
        assert!(!record_fenced(
            &malformed,
            &json!({"transport":"omp-channel", "ready":true, "image":42}).to_string()
        ));
        let beats = presence().beats.lock().unwrap();
        let beat = beats.get(recipient).unwrap();
        assert_eq!(beat.report.ready, Some(false));
        assert_eq!(beat.fence.as_ref().unwrap().epoch, 1);
    }

    #[test]
    fn attachment_requires_current_initialized_delivery_and_not_a_title_report() {
        let recipient = "agent/attachment-proof";
        let mut delivery = crate::mailbox::Fence::new(recipient, "current", "delivery");
        delivery.epoch = 1;
        let title = crate::mailbox::Fence::new(recipient, "current", "title");
        let ready = json!({"transport":"claude-channel", "ready":true,
            "channel":{"pid":8,"image":"new","age_ms":0}})
        .to_string();
        record_fenced(&title, &ready);
        assert!(attachment(recipient, "current").is_none());
        record_fenced(&delivery, &ready);
        assert_eq!(attachment(recipient, "current").unwrap().epoch, 1);
        assert!(attachment(recipient, "previous").is_none());
        record_fenced(
            &delivery,
            &json!({"transport":"claude-channel", "ready":false,
            "channel":{"pid":8,"age_ms":0}})
            .to_string(),
        );
        assert!(attachment(recipient, "current").is_none());
        record_fenced(&delivery, &ready);
        presence()
            .beats
            .lock()
            .unwrap()
            .get_mut(recipient)
            .unwrap()
            .at = Instant::now() - Duration::from_secs(11);
        assert!(attachment(recipient, "current").is_none());
    }

    #[test]
    fn a_missing_channel_has_visible_delivery_presence_and_legacy_polls_recover() {
        let recipient = "agent/attachment-missing";
        let title = crate::mailbox::Fence::new(recipient, "current", "title");
        record_fenced(
            &title,
            &json!({"transport":"claude-channel", "ready":false,
            "reason":"claude-channel-unattached: mail held"})
            .to_string(),
        );
        let assessment = known(recipient).unwrap();
        assert!(assessment.stale());
        assert!(reason(&assessment).contains("claude-channel-unattached"));
        record_legacy(recipient, "claude-channel", 7);
        assert_eq!(known(recipient).unwrap().state, "legacy");
    }

    fn report(image: Option<&str>, channel: Option<(Option<&str>, u64)>) -> Report {
        Report {
            transport: Some("claude-channel".into()),
            pid: Some(7),
            image: image.map(str::to_owned),
            follows: Some("/state/bin/st3".into()),
            follows_image: Some("new".into()),
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

    fn reason(assessment: &Assessment) -> &str {
        assessment.reason.as_deref().unwrap_or_default()
    }

    #[test]
    fn a_seat_without_a_poll_is_unknown_during_startup_and_stale_after() {
        let early = assess_beat(Duration::from_secs(3), DAEMON, None, "codex");
        assert_eq!(early.state, "unknown");
        let late = assess_beat(Duration::from_secs(60), DAEMON, None, "codex");
        assert!(late.stale(), "{late:?}");
    }

    #[test]
    fn a_live_poll_from_this_binary_is_current() {
        let assessment = assess_beat(
            Duration::from_secs(60),
            DAEMON,
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
            DAEMON,
            Some((Duration::from_secs(1), legacy.clone())),
            "omp",
        );
        assert_eq!(live.state, "legacy");
        assert!(!live.stale());
        assert!(live.reason.unwrap().contains("not reported"));
        let stopped = assess_beat(
            Duration::from_secs(60),
            DAEMON,
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
            DAEMON,
            Some((Duration::from_secs(1), report.clone())),
            "omp",
        );
        assert!(starting.stale());
        report.ready = Some(true);
        let ready = assess_beat(
            Duration::from_secs(60),
            DAEMON,
            Some((Duration::from_secs(1), report)),
            "omp",
        );
        assert_eq!(ready.state, "current");
    }

    #[test]
    fn a_ready_path_on_a_replaced_binary_is_outdated_and_says_whether_it_catches_up() {
        let following = report(Some("old"), None);
        let mut elsewhere = following.clone();
        elsewhere.follows = Some("/nix/store/old-st3/bin/st3".into());
        elsewhere.follows_image = Some("old".into());
        let mut collected = elsewhere.clone();
        collected.follows_image = None;
        let mut unreported = following.clone();
        unreported.follows = None;
        let restart =
            "follows /nix/store/old-st3/bin/st3, which is not the daemon's; restart the seat";
        for (report, driver, words) in [
            (
                following.clone(),
                "omp",
                "delivery process (pid 7) still runs a replaced st binary and is switching to the daemon's",
            ),
            (elsewhere, "omp", restart),
            (collected, "omp", restart),
            (
                unreported,
                "codex",
                "predates following the daemon's; restart the seat",
            ),
            (
                report(Some("new"), Some((Some("old"), 100))),
                "claude",
                "Claude channel (pid 8) still runs a replaced st binary and is switching",
            ),
        ] {
            let assessment = assess_beat(
                Duration::from_secs(60),
                DAEMON,
                Some((Duration::from_secs(1), report)),
                driver,
            );
            assert_eq!(assessment.state, "outdated", "{assessment:?}");
            assert!(!assessment.stale());
            assert!(
                reason(&assessment).contains(words),
                "{assessment:?} should say {words}"
            );
        }
        // Readiness and liveness still decide first: an old path that cannot hand off is stale.
        let mut refused = following;
        refused.ready = Some(false);
        let assessment = assess_beat(
            Duration::from_secs(60),
            DAEMON,
            Some((Duration::from_secs(1), refused)),
            "omp",
        );
        assert!(assessment.stale(), "{assessment:?}");
        let silent_channel = assess_beat(
            Duration::from_secs(60),
            DAEMON,
            Some((
                Duration::from_secs(1),
                report(Some("old"), Some((Some("old"), 60_000))),
            )),
            "claude",
        );
        assert!(silent_channel.stale(), "{silent_channel:?}");
    }

    #[test]
    fn a_silent_or_unidentified_delivery_path_is_stale() {
        for (beat, driver, words) in [
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
        ] {
            let assessment = assess_beat(Duration::from_secs(60), DAEMON, Some(beat), driver);
            assert!(assessment.stale(), "{assessment:?}");
            assert!(
                reason(&assessment).contains(words),
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
