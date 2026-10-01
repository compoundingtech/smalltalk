//! Explicit native delivery control supplied by the owning control plane.
//!
//! A graph permit is short lived and starts closed. A daemon outage can never leave an old
//! permission to hand off queued input standing forever. This has no filesystem transport.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Default)]
pub struct DeliveryGate(Arc<Mutex<Option<Instant>>>);

impl DeliveryGate {
    /// Update from a successful graph read. A hold closes the gate immediately.
    pub fn update(&self, held: bool, lease: Duration) {
        if let Ok(mut permit) = self.0.lock() {
            *permit = (!held).then(|| Instant::now() + lease);
        }
    }

    pub fn held(&self) -> bool {
        self.0.lock().map_or(true, |permit| {
            permit.is_none_or(|expires| Instant::now() >= expires)
        })
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
        gate.update(false, Duration::ZERO);
        assert!(control.held(&status));
    }
}
