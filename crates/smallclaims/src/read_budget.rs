//! Cooperative cancellation shared by an API read and its blocking workers.
//!
//! A budget never interrupts a writer. Read-pool connections consult it through their
//! progress handler; Rust reductions call `check` between items. Ending a future only
//! signals cancellation: the worker must reach one of these checkpoints to release its
//! resources. A single uninterruptible filesystem call or JSON parse is not preempted.

use std::cell::RefCell;
use std::panic::Location;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

#[derive(Clone)]
pub struct ReadBudget(Arc<State>);

struct State {
    parent: Option<ReadBudget>,
    deadline: Instant,
    route: Arc<str>,
    callsite: &'static Location<'static>,
    read_site: OnceLock<&'static Location<'static>>,
    cancelled: AtomicBool,
    reported: AtomicBool,
}

thread_local! {
    static CURRENT: RefCell<Option<ReadBudget>> = const { RefCell::new(None) };
}

impl ReadBudget {
    #[track_caller]
    pub fn new(route: impl Into<Arc<str>>, duration: Duration) -> Self {
        Self(Arc::new(State {
            parent: None,
            deadline: Instant::now() + duration,
            route: route.into(),
            callsite: Location::caller(),
            read_site: OnceLock::new(),
            cancelled: AtomicBool::new(false),
            reported: AtomicBool::new(false),
        }))
    }

    /// An event wait may last longer than one query. A child does not extend its parent.
    #[track_caller]
    pub fn child(&self, duration: Duration) -> Self {
        Self(Arc::new(State {
            parent: Some(self.clone()),
            deadline: self.0.deadline.min(Instant::now() + duration),
            route: self.0.route.clone(),
            callsite: Location::caller(),
            read_site: OnceLock::new(),
            cancelled: AtomicBool::new(false),
            reported: AtomicBool::new(false),
        }))
    }

    pub fn cancel(&self) {
        self.0.cancelled.store(true, Ordering::Release);
    }

    pub fn expired(&self) -> bool {
        self.0.cancelled.load(Ordering::Acquire)
            || Instant::now() >= self.0.deadline
            || self.0.parent.as_ref().is_some_and(Self::expired)
    }

    pub fn check(&self) -> Result<(), crate::error::Error> {
        if !self.expired() {
            return Ok(());
        }
        if !self.0.reported.swap(true, Ordering::AcqRel) {
            eprintln!(
                "st3: read cancelled route={:?} callsite={} deadline_elapsed={}",
                self.0.route,
                self.0.read_site.get().copied().unwrap_or(self.0.callsite),
                Instant::now() >= self.0.deadline,
            );
        }
        Err(crate::error::Error::new(
            "read-deadline",
            "the read exceeded its deadline or was cancelled",
        ))
    }

    pub fn route(&self) -> &str {
        &self.0.route
    }

    pub fn remaining(&self) -> Duration {
        self.0.deadline.saturating_duration_since(Instant::now())
    }
}

/// Pin the first concrete store read site for the query and enclosing request. This is
/// bounded per budget, not a per-subject registry or a result cache.
#[track_caller]
pub(crate) fn note_read_site() {
    let site = Location::caller();
    let mut budget = current();
    while let Some(current) = budget {
        let _ = current.0.read_site.set(site);
        budget = current.0.parent.clone();
    }
}

pub fn current() -> Option<ReadBudget> {
    CURRENT.with(|slot| slot.borrow().clone())
}

/// Restore the previous budget even when a worker unwinds. Never retain this thread-local
/// across an async task migration; API workers enter it on their dedicated blocking thread.
pub fn with<T>(budget: Option<ReadBudget>, work: impl FnOnce() -> T) -> T {
    struct Restore(Option<ReadBudget>);
    impl Drop for Restore {
        fn drop(&mut self) {
            CURRENT.with(|slot| *slot.borrow_mut() = self.0.take());
        }
    }
    let old = CURRENT.with(|slot| slot.replace(budget));
    let _restore = Restore(old);
    work()
}

pub fn check() -> Result<(), crate::error::Error> {
    current().map_or(Ok(()), |budget| budget.check())
}

/// Used only by a read connection's SQLite progress handler.
pub(crate) fn interrupted() -> bool {
    check().is_err()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellation_reaches_children_and_scope_restores_the_previous_budget() {
        let parent = ReadBudget::new("/parent", Duration::from_secs(30));
        let child = parent.child(Duration::from_secs(15));
        with(Some(parent.clone()), || {
            assert!(check().is_ok());
            with(Some(child.clone()), || {
                parent.cancel();
                assert_eq!(check().unwrap_err().code, "read-deadline");
            });
            assert_eq!(check().unwrap_err().code, "read-deadline");
        });
        assert!(current().is_none());
    }

    #[test]
    fn expired_budget_stops_a_rust_only_reduction() {
        let budget = ReadBudget::new("/cpu", Duration::ZERO);
        let mut items = 0;
        let result = with(Some(budget), || {
            for _ in 0..100_000 {
                check()?;
                items += 1;
            }
            Ok::<_, crate::error::Error>(())
        });
        assert_eq!(result.unwrap_err().code, "read-deadline");
        assert_eq!(items, 0);
    }
}
