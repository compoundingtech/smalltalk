//! Why a GitHub observer cannot observe, and when that needs a person. The reconciler classifies
//! a failed observation; the store's attention snapshot reads the same classification back.
use serde_json::Value;

use super::{ProviderForbidden, ProviderRateLimit, ProviderUnauthenticated};

/// Why an observer cannot observe, and whether a person has to act.
pub enum ObserverCondition {
    /// GitHub rejected the token or refused access. Nothing changes until a person acts.
    Access(String),
    /// A rate limit ends at a known reset. It needs a person only when the observer was already
    /// limited and is still limited after the reset that limit named.
    RateLimited { reason: String, outlasted: bool },
    /// Any other failure. A person looks at it once it has lasted an hour.
    Unreachable(String),
    /// The daemon rejected the observation itself. It is not polled again on this revision.
    Rejected(String),
}

/// How long after a named reset a rate limit counts as outlasting it, for clock skew.
pub const RATE_LIMIT_RESET_GRACE_MS: u128 = 60_000;
const OBSERVER_UNREACHABLE_ATTENTION_MS: u128 = 3_600_000;

impl ObserverCondition {
    pub fn of(error: &anyhow::Error, previous: Option<&Value>) -> Self {
        let reason = error.to_string();
        if error.downcast_ref::<ProviderForbidden>().is_some()
            || error.downcast_ref::<ProviderUnauthenticated>().is_some()
        {
            return Self::Access(reason);
        }
        if error.downcast_ref::<ProviderRateLimit>().is_some() {
            let outlasted = previous.is_some_and(|actual| {
                actual.get("state").and_then(Value::as_str) == Some("unreachable")
                    && actual.get("error_code").and_then(Value::as_str) == Some("rate-limited")
                    && actual
                        .get("next_check_unix_ms")
                        .and_then(Value::as_str)
                        .and_then(|value| value.parse::<u128>().ok())
                        .is_some_and(|reset| {
                            reset.saturating_add(RATE_LIMIT_RESET_GRACE_MS) <= smallclaims::store::now_ms()
                        })
            });
            return Self::RateLimited { reason, outlasted };
        }
        Self::Unreachable(reason)
    }

    pub fn code(&self) -> &'static str {
        match self {
            Self::Access(_) => "access-denied",
            Self::RateLimited { .. } => "rate-limited",
            Self::Unreachable(_) => "unreachable",
            Self::Rejected(_) => "rejected",
        }
    }

    pub fn needs_person(&self, failing_for_ms: u128) -> bool {
        match self {
            Self::Access(_) | Self::Rejected(_) => true,
            Self::RateLimited { outlasted, .. } => *outlasted,
            Self::Unreachable(_) => failing_for_ms >= OBSERVER_UNREACHABLE_ATTENTION_MS,
        }
    }

    pub fn title(&self) -> &'static str {
        match self {
            Self::Access(_) => "An observer has no access to GitHub",
            Self::RateLimited { .. } => "A GitHub rate limit outlasted its reset",
            Self::Unreachable(_) => "An observer has failed for an hour",
            Self::Rejected(_) => "st rejects an observer's observations",
        }
    }

    /// The cause and the command that fixes it. `host` polls the observer.
    pub fn reason(&self, subject: &str, host: &str) -> String {
        let closes = "This item closes when the observer observes again.";
        match self {
            Self::Access(reason) => format!(
                "{subject} cannot observe its resource: {reason}. Give the daemon account's GitHub token access to the repository, with `gh auth login` or `gh auth refresh -h github.com -s repo` as that account. The observer tries again on its own. {closes}"
            ),
            Self::RateLimited { reason, .. } => format!(
                "{subject} is still limited after the reset GitHub named: {reason}. Something else spends the GitHub budget that every host shares; `st doctor` on {host} shows how much of it remains, how much that host's observers spent, and each observer's requests. The observer waits for each reset on its own. {closes}"
            ),
            Self::Unreachable(reason) => format!(
                "{subject} has failed to observe for over an hour: {reason}. Inspect it with `st subject {subject}`. {closes}"
            ),
            Self::Rejected(reason) => format!(
                "st rejected an observation from {subject}: {reason}. Correct the observer's declaration and apply it again; inspect it with `st subject {subject}`. {closes}"
            ),
        }
    }
}
