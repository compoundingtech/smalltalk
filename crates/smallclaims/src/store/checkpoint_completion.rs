//! One allocator-reclamation attempt after an actual checkpoint/proof operation finishes.
//! Outer scopes defer it until their owned inputs and SQLite guards have dropped; nested
//! completion entrypoints still reclaim once. Polling/skipped work never arms a scope.
use std::cell::Cell;
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static RECLAIM_ATTEMPTS: AtomicU64 = AtomicU64::new(0);
static RECLAIM_NATIVE_CALLS: AtomicU64 = AtomicU64::new(0);
static RECLAIM_ELAPSED_NANOS: AtomicU64 = AtomicU64::new(0);

/// Process-lifetime totals for checkpoint allocator reclamation.
#[derive(Clone, Copy, Debug)]
pub struct AllocatorReclaimStats {
    pub attempts: u64,
    pub native_calls: u64,
    pub elapsed: Duration,
}

/// Independently sampled monotonic totals; a concurrent completion may be reflected
/// in one field before another. Reading these counters never calls the allocator.
pub fn allocator_reclaim_stats() -> AllocatorReclaimStats {
    AllocatorReclaimStats {
        attempts: RECLAIM_ATTEMPTS.load(Ordering::Relaxed),
        native_calls: RECLAIM_NATIVE_CALLS.load(Ordering::Relaxed),
        elapsed: Duration::from_nanos(RECLAIM_ELAPSED_NANOS.load(Ordering::Relaxed)),
    }
}

thread_local! {
    static COMPLETION: Cell<(usize, bool)> = const { Cell::new((0, false)) };
    #[cfg(any(test, feature = "test-support"))]
    static RECLAIMS: Cell<(u64, u64, std::time::Duration)> = const {
        Cell::new((0, 0, std::time::Duration::ZERO))
    };
}

pub(super) struct Completion {
    // Completion scopes are synchronous and must finish on their originating thread.
    _thread: PhantomData<Rc<()>>,
}

impl Completion {
    /// Defer nested work's reclamation until this caller has dropped its owned inputs.
    pub(super) fn outer() -> Self {
        Self::enter(false)
    }

    /// An actual proof or checkpoint application, including unsuccessful completion.
    pub(super) fn work() -> Self {
        Self::enter(true)
    }

    fn enter(work: bool) -> Self {
        COMPLETION.with(|state| {
            let (depth, pending) = state.get();
            state.set((depth + 1, pending || work));
        });
        Self { _thread: PhantomData }
    }
}

impl Drop for Completion {
    fn drop(&mut self) {
        let reclaim = COMPLETION.with(|state| {
            let (depth, pending) = state.get();
            debug_assert!(depth > 0);
            state.set((depth - 1, depth > 1 && pending));
            depth == 1 && pending
        });
        if reclaim {
            reclaim_allocator();
        }
    }
}

fn reclaim_allocator() {
    let started = Instant::now();
    platform_trim();
    let elapsed = started.elapsed();
    let native_calls = u64::from(cfg!(all(target_os = "linux", target_env = "gnu")));
    RECLAIM_ELAPSED_NANOS.fetch_add(
        u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX), Ordering::Relaxed,
    );
    RECLAIM_NATIVE_CALLS.fetch_add(native_calls, Ordering::Relaxed);
    RECLAIM_ATTEMPTS.fetch_add(1, Ordering::Relaxed);
    #[cfg(any(test, feature = "test-support"))]
    RECLAIMS.with(|state| {
        let (attempts, calls, previous_elapsed) = state.get();
        state.set((attempts + 1, calls + native_calls, previous_elapsed + elapsed));
    });
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn platform_trim() {
    // SAFETY: glibc's thread-safe malloc_trim only releases unused allocator pages;
    // padding zero retains no extra free pages and cannot invalidate live allocations.
    let _ = unsafe { libc::malloc_trim(0) };
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
use self::unsupported_platform_trim as platform_trim;

#[cfg(any(test, not(all(target_os = "linux", target_env = "gnu"))))]
fn unsupported_platform_trim() {}

/// Thread-local test totals preserve isolation between concurrent completion proofs.
#[cfg(any(test, feature = "test-support"))]
pub(super) fn reclaim_stats() -> (u64, u64, std::time::Duration) {
    RECLAIMS.with(Cell::get)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completion_increases_production_reclaim_counters() {
        let before = allocator_reclaim_stats();
        { let _work = Completion::work(); }
        let after = allocator_reclaim_stats();
        assert!(after.attempts > before.attempts);
        if cfg!(all(target_os = "linux", target_env = "gnu")) {
            assert!(after.native_calls > before.native_calls);
        } else {
            assert_eq!(after.native_calls, 0);
        }
        assert!(after.elapsed > before.elapsed);
    }

    #[test]
    fn nested_work_reclaims_once_after_the_outer_completion() {
        let before = reclaim_stats();
        {
            let outer = Completion::outer();
            { let _work = Completion::work(); }
            { let _nested_entrypoint = Completion::work(); }
            assert_eq!(reclaim_stats(), before);
            drop(outer);
        }
        let after = reclaim_stats();
        assert_eq!(after.0 - before.0, 1);
        assert_eq!(after.1 - before.1, u64::from(cfg!(all(target_os = "linux", target_env = "gnu"))));
    }

    #[test]
    fn skipped_outer_scope_never_reclaims() {
        let before = reclaim_stats();
        { let _skipped = Completion::outer(); }
        assert_eq!(reclaim_stats(), before);
    }

    #[test]
    fn failed_work_reclaims_once_after_unwinding() {
        let before = reclaim_stats();
        assert!(std::panic::catch_unwind(|| {
            let _completion = Completion::work();
            panic!("failed checkpoint work");
        }).is_err());
        assert_eq!(reclaim_stats().0 - before.0, 1);
    }

    #[test]
    fn unsupported_platform_implementation_is_a_noop() {
        let before = reclaim_stats();
        unsupported_platform_trim();
        assert_eq!(reclaim_stats(), before);
    }

    #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
    #[test]
    fn unsupported_completion_never_calls_glibc() {
        let before = reclaim_stats();
        { let _work = Completion::work(); }
        let after = reclaim_stats();
        assert_eq!(after.0 - before.0, 1);
        assert_eq!(after.1 - before.1, 0);
    }
}
