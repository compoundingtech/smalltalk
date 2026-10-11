//! Private, per-dialer permission for one off-cadence attempt. This does not reset backoff.

use super::*;
use std::error::Error as _;
use std::future::Future;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum HttpFailurePhase {
    BeforeHeaders,
    ResponseBody,
}

#[derive(Debug)]
pub(super) struct PeerHttpFailure {
    pub(super) phase: HttpFailurePhase,
    pub(super) cause: reqwest::Error,
}

impl std::fmt::Display for PeerHttpFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.cause)
    }
}
impl std::error::Error for PeerHttpFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.cause)
    }
}

impl PeerHttpFailure {
    pub(super) fn can_advance(error: &anyhow::Error) -> bool {
        let Some(failure) = error.downcast_ref::<Self>() else {
            return false;
        };
        let cause = &failure.cause;
        // Be conservative: HTTPS/TLS, builder, redirect, body, decode and status errors
        // never earn this exception. No headers does NOT mean no remote effect occurred.
        if failure.phase != HttpFailurePhase::BeforeHeaders
            || !cause.url().is_some_and(|url| url.scheme() == "http")
            || !cause.is_request()
        {
            return false;
        }
        if cause.is_timeout() {
            return true;
        }
        let mut source = cause.source();
        while let Some(error) = source {
            if let Some(io) = error.downcast_ref::<std::io::Error>()
                && matches!(
                    io.kind(),
                    std::io::ErrorKind::ConnectionRefused
                        | std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::ConnectionAborted
                )
            {
                return true;
            }
            source = error.source();
        }
        false
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct RetrySchedule {
    pub(super) started: tokio::time::Instant,
    pub(super) deadline: tokio::time::Instant,
    not_before: Option<tokio::time::Instant>,
}

impl RetrySchedule {
    pub(super) fn new(now: tokio::time::Instant, delay: Duration) -> Self {
        Self {
            started: now,
            deadline: now + delay,
            not_before: None,
        }
    }

    pub(super) fn transport_alive(&mut self) {
        self.deadline = self.deadline.min(self.started + Duration::from_secs(30));
        if let Some(not_before) = self.not_before {
            self.deadline = self.deadline.max(not_before);
        }
    }

    pub(super) fn honor_cooldown(&mut self, until: tokio::time::Instant) {
        self.not_before = Some(self.not_before.map_or(until, |before| before.max(until)));
        self.deadline = self.deadline.max(until);
    }

    pub(super) fn remaining(self) -> Duration {
        self.deadline
            .saturating_duration_since(tokio::time::Instant::now())
    }
}

pub(super) enum PeerRetryWake {
    Deadline,
    Changed,
    Advance(RecoveryAdvance),
}

impl PeerRetryWake {
    pub(super) fn into_advance(
        self,
        backoff: &mut PeerBackoff,
        credit: &mut RecoveryCredit,
    ) -> Option<RecoveryAdvance> {
        match self {
            Self::Advance(advance) => Some(advance),
            Self::Deadline => None,
            Self::Changed => {
                credit.invalidate();
                *backoff = PeerBackoff::default();
                None
            }
        }
    }
}

/// Not Clone: one task starts absent and spends the value before any awaited export.
#[derive(Default)]
pub(super) struct RecoveryCredit {
    earned: Option<(String, u64, tokio::time::Instant)>,
}

impl RecoveryCredit {
    pub(super) fn invalidate(&mut self) {
        self.earned = None;
    }

    pub(super) fn earn(
        &mut self,
        before: Option<u64>,
        fleet: &FleetContext,
        routes: &watch::Receiver<Vec<Route>>,
        url: &str,
    ) {
        self.invalidate();
        let _view = fleet.view.read().expect("fleet view lock poisoned");
        let generation = *fleet.view_changed.borrow();
        if before == Some(generation)
            && !fleet.is_removed()
            && !routes.has_changed().unwrap_or(true)
            && routes
                .borrow()
                .iter()
                .any(|route| matches!(route, Route::Http(current) if current == url))
        {
            self.earned = Some((
                url.to_owned(),
                generation,
                tokio::time::Instant::now() + PEER_PROBE_WINDOW,
            ));
        }
    }

    pub(super) fn spend(
        &mut self,
        fleet: &FleetContext,
        auth: &FleetAuth,
        routes: &watch::Receiver<Vec<Route>>,
        url: &str,
        mut schedule: RetrySchedule,
    ) -> Option<RecoveryAdvance> {
        schedule.transport_alive(); // carry the first HEAD cap even in a caller control
        if tokio::time::Instant::now() >= schedule.deadline {
            return None; // The ordinary attempt is already due, not an off-cadence advance.
        }
        // Take even an invalid/expired value: a notification can never revive it.
        let (earned_url, generation, expires) = self.earned.take()?;
        let _view = fleet.view.read().expect("fleet view lock poisoned");
        let current_routes = routes.borrow();
        if earned_url != url
            || fleet.is_removed()
            || *fleet.view_changed.borrow() != generation
            || routes.has_changed().unwrap_or(true)
            || !current_routes
                .iter()
                .any(|route| matches!(route, Route::Http(current) if current == url))
            || tokio::time::Instant::now() >= expires
        {
            return None;
        }
        Some(RecoveryAdvance {
            schedule,
            generation,
            expires,
            url: url.to_owned(),
            routes: current_routes.clone(),
            view: fleet.view.clone(),
            auth: auth.clone(),
        })
    }
}

/// Scalar generation read under the same guard that publishes membership changes.
pub(super) fn retry_generation(
    fleet: &FleetContext,
    routes: &watch::Receiver<Vec<Route>>,
    url: &str,
) -> Option<u64> {
    let _view = fleet.view.read().expect("fleet view lock poisoned");
    (!fleet.is_removed()
        && !routes.has_changed().unwrap_or(true)
        && routes
            .borrow()
            .iter()
            .any(|route| matches!(route, Route::Http(current) if current == url)))
    .then(|| *fleet.view_changed.borrow())
}

/// Admission refusal is not a checkpoint failure and must not poison adoption cooldown.
#[derive(Debug)]
pub(super) struct RetryAdmissionInvalidated;
impl std::fmt::Display for RetryAdmissionInvalidated {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("advanced retry authority, route or lifetime changed")
    }
}
impl std::error::Error for RetryAdmissionInvalidated {}

/// Owned by one advanced exchange; cancellation drops it without restoring the credit.
pub(super) struct RecoveryAdvance {
    pub(super) schedule: RetrySchedule,
    generation: u64,
    expires: tokio::time::Instant,
    pub(super) url: String,
    routes: Vec<Route>,
    view: Arc<std::sync::RwLock<FleetView>>,
    auth: FleetAuth,
}

impl RecoveryAdvance {
    pub(super) fn check(
        &self,
        fleet: &FleetContext,
        auth: &FleetAuth,
        routes: &watch::Receiver<Vec<Route>>,
        url: &str,
    ) -> Result<()> {
        let _view = fleet.view.read().expect("fleet view lock poisoned");
        let same_member = match (&self.auth.member, &auth.member) {
            (None, None) => true,
            (Some(before), Some(after)) => Arc::ptr_eq(before, after),
            _ => false,
        };
        if !(Arc::ptr_eq(&self.view, &fleet.view)
            && self.auth.fleet_id() == auth.fleet_id()
            && Arc::ptr_eq(&self.auth.secret, &auth.secret)
            && same_member
            && !fleet.is_removed()
            && *fleet.view_changed.borrow() == self.generation
            && !routes.has_changed().unwrap_or(true)
            && *routes.borrow() == self.routes
            && self.url == url
            && tokio::time::Instant::now() < self.expires)
        {
            return Err(RetryAdmissionInvalidated.into());
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
pub(super) struct RetrySubmission<'a> {
    pub(super) advance: &'a RecoveryAdvance,
    pub(super) routes: &'a watch::Receiver<Vec<Route>>,
}

impl RetrySubmission<'_> {
    pub(super) fn check(self, fleet: &FleetContext, auth: &FleetAuth, url: &str) -> Result<()> {
        self.advance.check(fleet, auth, self.routes, url)
    }
}

/// The actual export caller, with no guard retained across the future. Construct the future
/// only after admission, and recheck after an arbitrarily slow or cancelling export.
pub(super) async fn retry_export<T, F: Future<Output = Result<T>>>(
    submission: Option<RetrySubmission<'_>>,
    fleet: &FleetContext,
    auth: &FleetAuth,
    url: &str,
    export: impl FnOnce() -> F,
) -> Result<T> {
    if let Some(submission) = submission {
        submission.check(fleet, auth, url)?;
    }
    let value = export().await?;
    if let Some(submission) = submission {
        submission.check(fleet, auth, url)?;
    }
    Ok(value)
}

/// The actual submission caller; serializing/signing keeps its original checks and this
/// fence runs immediately before send, after export and worker-status awaits.
pub(super) async fn retry_submit<T, F: Future<Output = Result<T>>>(
    submission: Option<RetrySubmission<'_>>,
    fleet: &FleetContext,
    auth: &FleetAuth,
    url: &str,
    send: impl FnOnce() -> F,
) -> Result<T> {
    if let Some(submission) = submission {
        submission.check(fleet, auth, url)?;
    }
    send().await
}

pub(super) enum FailedRetryPlan {
    Ordinary(Duration),
    Resumed(RetrySchedule),
}

impl FailedRetryPlan {
    pub(super) fn remaining(&self) -> Duration {
        match self {
            Self::Ordinary(delay) => *delay,
            Self::Resumed(schedule) => schedule.remaining(),
        }
    }

    pub(super) fn after_visibility(self, now: tokio::time::Instant) -> RetrySchedule {
        match self {
            Self::Ordinary(delay) => RetrySchedule::new(now, delay),
            Self::Resumed(schedule) => schedule,
        }
    }
}

/// Used by the dialer itself, not just the policy controls. The ordinary wait still
/// starts after its visibility await; a resumed absolute deadline never moves with it.
pub(super) fn failed_retry_plan(
    backoff: &mut PeerBackoff,
    resumed: Option<RetrySchedule>,
    ordinary_delay: impl FnOnce(&mut PeerBackoff) -> Duration,
) -> FailedRetryPlan {
    match resumed {
        Some(schedule) => FailedRetryPlan::Resumed(schedule),
        None => FailedRetryPlan::Ordinary(ordinary_delay(backoff)),
    }
}

#[cfg(test)]
#[path = "worker_retry_tests.rs"]
mod tests;
