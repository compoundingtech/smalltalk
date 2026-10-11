//! Explicit native delivery control supplied by the owning control plane.
//!
//! A graph permit is short lived and starts closed. A daemon outage can never leave an old
//! permission to hand off queued input standing forever. This has no filesystem transport.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Why new native input cannot be handed off, independently of mailbox liveness.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeliveryBlockReason {
    ControlUnavailable,
    HoldActive,
    Idle,
}

impl DeliveryBlockReason {
    pub fn description(self) -> &'static str {
        match self {
            Self::ControlUnavailable => {
                "delivery-control-unavailable: new native handoffs are held until graph delivery control can be verified; the driver retries automatically"
            }
            Self::HoldActive => {
                "delivery-hold-active: new native handoffs are held by graph delivery control"
            }
            Self::Idle => "delivery-idle: no native mail awaits handoff",
        }
    }
}

#[derive(Debug, Default)]
struct Permit {
    expires: Option<Instant>,
    blocked: Option<DeliveryBlockReason>,
}

#[derive(Clone, Debug, Default)]
pub struct DeliveryGate(Arc<Mutex<Permit>>);

impl DeliveryGate {
    /// Empty mail needs no permission. This closes the gate without claiming a failed read.
    pub fn idle(&self) {
        if let Ok(mut permit) = self.0.lock() {
            permit.expires = None;
            permit.blocked = Some(DeliveryBlockReason::Idle);
        }
    }

    /// Update from a successful graph read. A hold closes the gate immediately.
    pub fn update(&self, held: bool, lease: Duration) {
        if let Ok(mut permit) = self.0.lock() {
            permit.expires = (!held).then(|| Instant::now() + lease);
            permit.blocked = held.then_some(DeliveryBlockReason::HoldActive);
        }
    }

    /// A failed read immediately withdraws any previous permission.
    pub fn unavailable(&self) {
        if let Ok(mut permit) = self.0.lock() {
            permit.expires = None;
            permit.blocked = Some(DeliveryBlockReason::ControlUnavailable);
        }
    }

    pub fn blocked_reason(&self) -> Option<DeliveryBlockReason> {
        self.0
            .lock()
            .map_or(Some(DeliveryBlockReason::ControlUnavailable), |permit| {
                if permit
                    .expires
                    .is_some_and(|expires| Instant::now() < expires)
                {
                    None
                } else {
                    Some(
                        permit
                            .blocked
                            .unwrap_or(DeliveryBlockReason::ControlUnavailable),
                    )
                }
            })
    }

    pub fn held(&self) -> bool {
        self.blocked_reason().is_some()
    }
}

/// Where a native prompt stands in st: still open, answered by the seat's person (and how), or
/// gone. The same shape as the daemon's `GET /v1/harness-prompts/state`.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(tag = "state", rename_all = "kebab-case")]
pub enum PromptAnswer {
    Open,
    Answered { answer: String },
    Gone,
}

type PromptAnswerRead = dyn Fn(u64, u64) -> Option<PromptAnswer> + Send + Sync;

/// How a native driver reads a person's answer to its prompt, named by the (ownership,
/// transition) sequences of the state record observation the prompt wrote. The owning control
/// plane supplies it; `None` means st could not say this time.
#[derive(Clone)]
pub struct PromptAnswers(Arc<PromptAnswerRead>);

impl PromptAnswers {
    pub fn new(read: impl Fn(u64, u64) -> Option<PromptAnswer> + Send + Sync + 'static) -> Self {
        Self(Arc::new(read))
    }

    pub(crate) fn read(&self, ownership: u64, transition: u64) -> Option<PromptAnswer> {
        (self.0)(ownership, transition)
    }
}

impl std::fmt::Debug for PromptAnswers {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("PromptAnswers")
    }
}

/// Catalog drivers retain their product's status transport. Native drivers take graph control.
#[derive(Clone, Debug, Default)]
pub enum SessionControl {
    #[default]
    Catalog,
    Graph(DeliveryGate),
}

impl SessionControl {
    pub(crate) fn refresh(&self, status_path: &Path) {
        if matches!(self, Self::Catalog) {
            let _ = crate::status::refresh(status_path);
        }
    }

    pub(crate) fn held(&self, status_path: &Path) -> bool {
        match self {
            Self::Catalog => crate::status::read_state(status_path) == crate::status::State::Dnd,
            Self::Graph(gate) => gate.held(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_control_starts_closed_expires_and_never_reads_or_refreshes_status() {
        let root = tempfile::tempdir().unwrap();
        let status = root.path().join("status");
        let gate = DeliveryGate::default();
        let control = SessionControl::Graph(gate.clone());
        assert!(control.held(&status));
        assert_eq!(
            gate.blocked_reason(),
            Some(DeliveryBlockReason::ControlUnavailable)
        );
        control.refresh(&status);
        assert!(!status.exists());
        crate::status::set_state(&status, crate::status::State::Dnd).unwrap();
        let before = std::fs::read(&status).unwrap();
        gate.update(false, Duration::from_secs(10));
        assert!(!control.held(&status));
        control.refresh(&status);
        assert_eq!(std::fs::read(&status).unwrap(), before);
        gate.update(true, Duration::from_secs(10));
        assert!(control.held(&status));
        assert_eq!(gate.blocked_reason(), Some(DeliveryBlockReason::HoldActive));
        gate.unavailable();
        assert_eq!(
            gate.blocked_reason(),
            Some(DeliveryBlockReason::ControlUnavailable)
        );
        gate.update(false, Duration::ZERO);
        assert!(control.held(&status));
        gate.idle();
        assert!(control.held(&status));
        assert_eq!(gate.blocked_reason(), Some(DeliveryBlockReason::Idle));
        gate.update(false, Duration::from_secs(10));
        assert!(!control.held(&status), "fresh mail needs a new graph permit");
        gate.unavailable();
        assert_eq!(
            gate.blocked_reason(),
            Some(DeliveryBlockReason::ControlUnavailable)
        );
    }
}
