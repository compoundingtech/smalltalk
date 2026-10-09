//! One allocator-reclamation attempt after an actual checkpoint/proof operation finishes.
//! Outer scopes defer it until their owned inputs and SQLite guards have dropped; nested
//! completion entrypoints still reclaim once. Polling/skipped work never arms a scope.
use std::cell::Cell;
use std::marker::PhantomData;
use std::rc::Rc;

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
    #[cfg(any(test, feature = "test-support"))]
    let started = std::time::Instant::now();
    platform_trim();
    #[cfg(any(test, feature = "test-support"))]
    RECLAIMS.with(|state| {
        let (attempts, native_calls, elapsed) = state.get();
        state.set((attempts + 1,
            native_calls + u64::from(cfg!(all(target_os = "linux", target_env = "gnu"))),
            elapsed + started.elapsed()));
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

/// Attempts, actual glibc calls, and time spent reclaiming on this thread. No production
/// timing or counters are installed unless the existing test-support feature is enabled.
#[cfg(any(test, feature = "test-support"))]
pub(super) fn reclaim_stats() -> (u64, u64, std::time::Duration) {
    RECLAIMS.with(Cell::get)
}

#[cfg(test)]
mod tests {
    use super::*;

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
