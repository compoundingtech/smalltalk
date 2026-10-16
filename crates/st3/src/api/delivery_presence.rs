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
use tokio::sync::watch;

/// A seat gets this long after the daemon starts before a missing poll counts as stale. Drivers
/// retry every second while the daemon restarts, and a replaced driver re-executes within two.
const STARTUP_GRACE: Duration = Duration::from_secs(20);
/// Delivery polls every second and backs off to at most 30 seconds on a failing daemon call.
const POLL_STALE_AFTER: Duration = Duration::from_secs(45);
/// The Claude channel refreshes its presence every second.
const CHANNEL_STALE_AFTER_MS: u64 = 10_000;
const CHANNEL_STALE_AFTER: Duration = Duration::from_millis(CHANNEL_STALE_AFTER_MS);

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
    /// Tokio's clock, so a paused test clock and a stream's deadline share one timeline.
    at: tokio::time::Instant,
    report: Report,
    fence: Option<crate::mailbox::Fence>,
}

struct Presence {
    started: Instant,
    image: Option<String>,
    beats: Mutex<HashMap<String, Beat>>,
    monitors: Mutex<HashMap<String, Beat>>,
    /// Attachment-push title streams waiting on a subject, one sender per watched subject.
    watchers: Mutex<HashMap<String, watch::Sender<u64>>>,
    /// The title binding of each live stream that has been sent an attachment frame.
    pushed: Mutex<HashMap<String, crate::mailbox::Fence>>,
}

fn presence() -> &'static Presence {
    static PRESENCE: OnceLock<Presence> = OnceLock::new();
    PRESENCE.get_or_init(|| Presence {
        started: Instant::now(),
        image: st_drivers::reexec::running_identity().map(|identity| identity.token()),
        beats: Mutex::new(HashMap::new()),
        monitors: Mutex::new(HashMap::new()),
        watchers: Mutex::new(HashMap::new()),
        pushed: Mutex::new(HashMap::new()),
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
    replace_beat(
        recipient,
        Beat {
            at: tokio::time::Instant::now(),
            report,
            fence: None,
        },
    );
}

/// Title updates cannot establish delivery readiness. They can report the outer driver's
/// attachment check, including a missing plugin for which no delivery process exists.
pub(super) fn record_fenced(fence: &crate::mailbox::Fence, raw: &str) -> bool {
    let Ok(report) = serde_json::from_str::<Report>(raw) else {
        return false;
    };
    let beat = Beat {
        at: tokio::time::Instant::now(),
        report,
        fence: Some(fence.clone()),
    };
    if fence.component == "delivery" {
        if beat.report.transport.as_deref() != Some("claude-channel")
            && let Ok(mut monitors) = presence().monitors.lock()
        {
            monitors.remove(&fence.subject);
        }
        return replace_beat(&fence.subject, beat);
    }
    if beat.report.transport.as_deref() != Some("claude-channel") {
        return false;
    }
    if let Ok(mut monitors) = presence().monitors.lock() {
        monitors.insert(fence.subject.clone(), beat);
        return true;
    }
    false
}

pub(super) fn attachment(recipient: &str, incarnation: &str) -> Option<crate::mailbox::Fence> {
    attachment_window(recipient, incarnation)
        .filter(|(_, until)| tokio::time::Instant::now() <= *until)
        .map(|(fence, _)| fence)
}

/// The one attachment predicate, shared by the poll and by the push stream's deadline: the
/// delivery binding a beat proves and the instant that proof lapses. The report's `age_ms` is
/// fixed when the channel sends it, so it gates the proof without moving the deadline.
fn window<'a>(
    beat: &'a Beat,
    incarnation: &str,
) -> Option<(&'a crate::mailbox::Fence, tokio::time::Instant)> {
    let fence = beat.fence.as_ref()?;
    (fence.incarnation == incarnation
        && beat.report.transport.as_deref() == Some("claude-channel")
        && beat.report.ready == Some(true)
        && beat.report.channel.as_ref().is_some_and(|channel| {
            channel
                .age_ms
                .is_some_and(|age| age <= CHANNEL_STALE_AFTER_MS)
        }))
    .then(|| (fence, beat.at + stale_after(&fence.subject)))
}

/// How long a beat proves attachment. Always the constant in production; a test may shorten it
/// for its own recipient, and the poll and the deadline still read the same window.
fn stale_after(recipient: &str) -> Duration {
    #[cfg(test)]
    if let Some(window) = WINDOW_OVERRIDES
        .lock()
        .ok()
        .and_then(|overrides| overrides.get(recipient).copied())
    {
        return window;
    }
    let _ = recipient;
    CHANNEL_STALE_AFTER
}

#[cfg(test)]
static WINDOW_OVERRIDES: std::sync::LazyLock<Mutex<HashMap<String, Duration>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

/// Shorten the proof window for one test recipient, so a stream test need not wait ten seconds.
#[cfg(test)]
pub(super) fn shorten_window(recipient: &str, window: Duration) {
    WINDOW_OVERRIDES
        .lock()
        .unwrap()
        .insert(recipient.to_owned(), window);
}

/// The proving delivery binding for `incarnation` and when it lapses, whether or not it has.
pub(super) fn attachment_window(
    recipient: &str,
    incarnation: &str,
) -> Option<(crate::mailbox::Fence, tokio::time::Instant)> {
    let beats = presence().beats.lock().ok()?;
    let (fence, until) = window(beats.get(recipient)?, incarnation)?;
    Some((fence.clone(), until))
}

/// What a beat proves at `now`, under any incarnation. Watchers look again only when it changes;
/// a beat that merely refreshes the same proof leaves it equal.
fn proof(beat: Option<&Beat>, now: tokio::time::Instant) -> Option<(String, u64, String)> {
    let beat = beat?;
    let incarnation = beat.fence.as_ref()?.incarnation.as_str();
    window(beat, incarnation)
        .filter(|(_, until)| now <= *until)
        .map(|(fence, _)| (fence.incarnation.clone(), fence.epoch, fence.token.clone()))
}

/// Replace a recipient's delivery beat, and wake its attachment watchers if what it proves
/// changed: an attach, a ready flip, a new binding, or a fresh proof after a lapsed one.
fn replace_beat(recipient: &str, beat: Beat) -> bool {
    let Ok(mut beats) = presence().beats.lock() else {
        return false;
    };
    let now = tokio::time::Instant::now();
    let changed = proof(beats.get(recipient), now) != proof(Some(&beat), now);
    beats.insert(recipient.to_owned(), beat);
    drop(beats);
    if changed {
        notify(recipient);
    }
    true
}

fn notify(recipient: &str) {
    if let Ok(watchers) = presence().watchers.lock()
        && let Some(sender) = watchers.get(recipient)
    {
        sender.send_modify(|generation| *generation = generation.wrapping_add(1));
    }
}

fn same_binding(a: &crate::mailbox::Fence, b: &crate::mailbox::Fence) -> bool {
    a.subject == b.subject
        && a.incarnation == b.incarnation
        && a.component == b.component
        && a.epoch == b.epoch
        && a.token == b.token
}

/// One push title stream's subscription to its subject's attachment changes. The map keeps a
/// subject only while a live stream watches it; dropping the last watch removes the entry.
pub(super) struct AttachmentWatch {
    subject: String,
    receiver: Option<watch::Receiver<u64>>,
}

impl AttachmentWatch {
    /// Wait for the next change. Never resolves if the watch could not be registered.
    pub(super) async fn changed(&mut self) {
        let Some(receiver) = self.receiver.as_mut() else {
            return std::future::pending().await;
        };
        if receiver.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

impl Drop for AttachmentWatch {
    fn drop(&mut self) {
        let Ok(mut watchers) = presence().watchers.lock() else {
            return;
        };
        drop(self.receiver.take());
        if watchers
            .get(&self.subject)
            .is_some_and(|sender| sender.receiver_count() == 0)
        {
            watchers.remove(&self.subject);
        }
    }
}

pub(super) fn watch_attachment(subject: &str) -> AttachmentWatch {
    let receiver = presence().watchers.lock().ok().map(|mut watchers| {
        watchers
            .entry(subject.to_owned())
            .or_insert_with(|| watch::channel(0).0)
            .subscribe()
    });
    AttachmentWatch {
        subject: subject.to_owned(),
        receiver,
    }
}

#[cfg(test)]
pub(super) fn watched(subject: &str) -> bool {
    presence()
        .watchers
        .lock()
        .is_ok_and(|watchers| watchers.contains_key(subject))
}

/// Age a recipient's delivery beat, or its title monitor, as if it arrived `by` earlier.
#[cfg(test)]
pub(super) fn backdate(recipient: &str, monitor: bool, by: Duration) {
    let presence = presence();
    let mut beats = if monitor {
        presence.monitors.lock()
    } else {
        presence.beats.lock()
    }
    .unwrap();
    let beat = beats.get_mut(recipient).expect("a recorded beat");
    beat.at -= by;
}

#[cfg(test)]
impl AttachmentWatch {
    pub(super) fn has_changed(&self) -> bool {
        self.receiver
            .as_ref()
            .is_some_and(|receiver| receiver.has_changed().unwrap_or(false))
    }
}

/// Record that this daemon sent a live title stream an attachment frame.
pub(super) fn note_pushed(fence: &crate::mailbox::Fence) {
    if let Ok(mut pushed) = presence().pushed.lock() {
        pushed.insert(fence.subject.clone(), fence.clone());
    }
}

/// The title stream ended; forget it unless a newer binding has already been pushed to.
pub(super) fn forget_pushed(fence: &crate::mailbox::Fence) {
    if let Ok(mut pushed) = presence().pushed.lock()
        && pushed
            .get(&fence.subject)
            .is_some_and(|held| same_binding(held, fence))
    {
        pushed.remove(&fence.subject);
    }
}

/// Whether this exact title binding has a live stream that negotiated push and was sent a
/// frame. This proves only a push-capable binding, not that a request from it was an admission.
pub(super) fn push_capable(fence: &crate::mailbox::Fence) -> bool {
    presence().pushed.lock().is_ok_and(|pushed| {
        pushed
            .get(&fence.subject)
            .is_some_and(|held| same_binding(held, fence))
    })
}

/// A Claude seat's attachment-push inventory: `negotiated` when its current title stream has
/// been pushed to, `not-negotiated` when a fresh title report exists without push, otherwise
/// `unknown`.
pub(crate) fn attachment_push(recipient: &str, incarnation: Option<&str>) -> &'static str {
    let current = |fence: &crate::mailbox::Fence| {
        incarnation.is_none_or(|incarnation| fence.incarnation == incarnation)
    };
    if presence()
        .pushed
        .lock()
        .is_ok_and(|pushed| pushed.get(recipient).is_some_and(current))
    {
        return "negotiated";
    }
    let reported = presence().monitors.lock().is_ok_and(|monitors| {
        monitors.get(recipient).is_some_and(|beat| {
            beat.at.elapsed() <= POLL_STALE_AFTER && beat.fence.as_ref().is_some_and(current)
        })
    });
    if reported {
        "not-negotiated"
    } else {
        "unknown"
    }
}

/// A metadata-free poll from a Unix peer proven to be this seat's native delivery process.
/// This proves liveness, not that an old executable matches the installed binary.
pub(crate) fn record_legacy(recipient: &str, transport: &str, pid: u32) {
    // A provider launched by an older driver has no title-side attachment monitor.
    if let Ok(mut monitors) = presence().monitors.lock() {
        monitors.remove(recipient);
    }
    replace_beat(
        recipient,
        Beat {
            at: tokio::time::Instant::now(),
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
    assess_selected(recipient, driver, None, false)
}

pub(crate) fn assess_current(recipient: &str, driver: &str, incarnation: Option<&str>) -> Assessment {
    assess_selected(recipient, driver, incarnation, true)
}

fn assess_selected(recipient: &str, driver: &str, incarnation: Option<&str>, current: bool) -> Assessment {
    let presence = presence();
    let eligible = |beat: &&Beat| {
        if !current { return true; }
        let transport = match driver {
            "claude" => "claude-channel", "codex" => "app-server", "opencode" => "opencode-server",
            "omp" => "omp-channel", "pi" => "pi-channel", _ => return false,
        };
        beat.report.transport.as_deref() == Some(transport)
            && beat.fence.as_ref().is_none_or(|fence| incarnation.is_none_or(|incarnation| fence.incarnation == incarnation))
    };
    let captured = |beat: &Beat| {
        let mut report = beat.report.clone();
        if current && beat.fence.is_none() && report.ready != Some(false) {
            report.legacy = true;
        }
        (beat.at, report)
    };
    let mut beat = presence.beats.lock().ok().and_then(|beats| {
        beats
            .get(recipient)
            .filter(eligible)
            .map(captured)
    });
    if driver == "claude"
        && let Ok(monitors) = presence.monitors.lock()
        && let Some(monitor) = monitors.get(recipient).filter(eligible)
        && monitor.report.ready == Some(false)
    {
        beat = Some(captured(monitor));
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
            .at = tokio::time::Instant::now() - Duration::from_secs(11);
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
        // A legacy poll proves transport liveness without asserting a fenced incarnation.
        assert_eq!(assess_current(recipient, "claude", Some("current")).state, "legacy");
        assert!(assess_current(recipient, "codex", Some("current")).state != "legacy");
    }

    #[test]
    fn unfenced_reports_preserve_legacy_liveness_but_never_current_native_readiness() {
        let recipient = "agent/eval.unfenced-report";
        record(recipient, &json!({"transport":"omp-channel","pid":7,"ready":true}).to_string());
        assert_eq!(assess_current(recipient, "omp", Some("current")).state, "legacy");
        assert_ne!(assess_current(recipient, "codex", Some("current")).state, "legacy");
        let old = crate::mailbox::Fence::new(recipient, "old", "delivery");
        record_fenced(&old, &json!({"transport":"omp-channel","pid":7,"ready":true}).to_string());
        let current = assess_current(recipient, "omp", Some("current"));
        assert!(matches!(current.state, "unknown" | "stale"));
        assert_eq!(current.transport, None);
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

    fn claude_report(ready: bool, age_ms: u64) -> String {
        json!({"transport":"claude-channel", "ready":ready,
            "channel":{"pid":8,"image":"new","age_ms":age_ms}})
        .to_string()
    }

    // The poll and the push stream's deadline read one predicate: attached through
    // `at + 10 s` inclusive, and the report's own age is a gate fixed when it was sent.
    #[tokio::test(start_paused = true)]
    async fn the_poll_and_the_deadline_share_one_predicate() {
        let recipient = "agent/attachment-window";
        let mut delivery = crate::mailbox::Fence::new(recipient, "current", "delivery");
        delivery.epoch = 3;
        for (age_ms, attached) in [(9_999, true), (10_000, true), (10_001, false)] {
            record_fenced(&delivery, &claude_report(true, age_ms));
            assert_eq!(
                attachment(recipient, "current").is_some(),
                attached,
                "{age_ms}"
            );
            assert_eq!(
                attachment_window(recipient, "current").is_some(),
                attached,
                "{age_ms}"
            );
        }
        record_fenced(&delivery, &claude_report(true, 0));
        let (fence, until) = attachment_window(recipient, "current").unwrap();
        assert_eq!(fence.epoch, 3);
        assert_eq!(until, tokio::time::Instant::now() + CHANNEL_STALE_AFTER);
        tokio::time::advance(CHANNEL_STALE_AFTER - Duration::from_millis(1)).await;
        assert!(attachment(recipient, "current").is_some());
        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(
            attachment(recipient, "current").is_some(),
            "the last instant still holds"
        );
        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(attachment(recipient, "current").is_none());
        assert_eq!(
            attachment_window(recipient, "current").unwrap().1,
            until,
            "a lapse moves no deadline"
        );
    }

    // Planned T11: a beat that only refreshes the same proof wakes no push stream, so a
    // steadily attached seat costs its title stream nothing until the deadline.
    #[test]
    fn only_a_change_in_what_presence_proves_wakes_a_watcher() {
        let recipient = "agent/attachment-watch-wakes";
        let mut delivery = crate::mailbox::Fence::new(recipient, "current", "delivery");
        delivery.epoch = 1;
        let watch = watch_attachment(recipient);
        assert!(!watch.has_changed());
        record_fenced(&delivery, &claude_report(true, 0));
        assert!(watch.has_changed(), "an attach wakes");
        let mut watch = watch;
        if let Some(receiver) = watch.receiver.as_mut() {
            receiver.borrow_and_update();
        }
        for _ in 0..1_000 {
            record_fenced(&delivery, &claude_report(true, 0));
        }
        assert!(
            !watch.has_changed(),
            "refreshing the same proof wakes nothing"
        );
        record_fenced(&delivery, &claude_report(false, 0));
        assert!(watch.has_changed(), "a ready flip wakes");
        if let Some(receiver) = watch.receiver.as_mut() {
            receiver.borrow_and_update();
        }
        let mut rebound = delivery.clone();
        rebound.epoch = 2;
        record_fenced(&rebound, &claude_report(true, 0));
        assert!(watch.has_changed(), "a new binding wakes");
        if let Some(receiver) = watch.receiver.as_mut() {
            receiver.borrow_and_update();
        }
        backdate(recipient, false, Duration::from_secs(11));
        record_fenced(&rebound, &claude_report(true, 0));
        assert!(
            watch.has_changed(),
            "a fresh proof after a lapsed one wakes"
        );
    }

    // Planned T14: the watcher map holds a subject only while a live watch exists, and a
    // change for a subject nobody watches creates no entry.
    #[test]
    fn watchers_exist_only_for_live_watches() {
        let watches = (0..1_000)
            .map(|seat| watch_attachment(&format!("agent/attachment-watch-bound-{seat}")))
            .collect::<Vec<_>>();
        assert!(watched("agent/attachment-watch-bound-999"));
        drop(watches);
        assert!((0..1_000).all(|seat| !watched(&format!("agent/attachment-watch-bound-{seat}"))));
        for seat in 0..1_000 {
            let recipient = format!("agent/attachment-unwatched-{seat}");
            let mut delivery = crate::mailbox::Fence::new(&recipient, "current", "delivery");
            delivery.epoch = 1;
            record_fenced(&delivery, &claude_report(true, 0));
            assert!(!watched(&recipient));
        }
        let recipient = "agent/attachment-watch-shared";
        let first = watch_attachment(recipient);
        let second = watch_attachment(recipient);
        drop(first);
        assert!(watched(recipient), "a second live watch keeps the entry");
        drop(second);
        assert!(!watched(recipient));
    }

    // Planned T13: once the driver reports only on change and at each 10-second Ping, its title
    // report is up to about ten seconds old. That age never changes what the seat shows: an
    // unattached monitor is stale at any age, and an attached one is never consulted.
    #[test]
    fn title_report_age_never_changes_the_assessment() {
        let recipient = "agent/attachment-monitor-age";
        let mut delivery = crate::mailbox::Fence::new(recipient, "current", "delivery");
        delivery.epoch = 1;
        let mut title = crate::mailbox::Fence::new(recipient, "current", "title");
        title.epoch = 1;
        record_fenced(&delivery, &claude_report(true, 0));
        let without_monitor = assess_current(recipient, "claude", Some("current"));
        record_fenced(
            &title,
            &json!({"transport":"claude-channel", "ready":false,
                "reason":"claude-channel-unattached: mail held"})
            .to_string(),
        );
        let mut aged = Duration::ZERO;
        for age in [0, 9, 11, 44, 46] {
            let age = Duration::from_secs(age);
            backdate(recipient, true, age - aged);
            aged = age;
            let assessment = assess_current(recipient, "claude", Some("current"));
            assert!(assessment.stale(), "{age:?}: {assessment:?}");
        }
        record_fenced(
            &title,
            &json!({"transport":"claude-channel", "ready":true}).to_string(),
        );
        for age in [0, 11, 46] {
            backdate(recipient, true, Duration::from_secs(age));
            let mut assessment = assess_current(recipient, "claude", Some("current"));
            let mut expected = without_monitor.clone();
            // Only the delivery beat's own age moves between the two readings.
            assessment.polled_seconds_ago = None;
            expected.polled_seconds_ago = None;
            assert_eq!(assessment, expected, "{age}");
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
