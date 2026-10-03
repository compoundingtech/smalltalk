//! What a piece of work on this thread read and whether it wrote.
//!
//! The reconciler evaluates each item (a mission run, a member, an intake declaration) on one
//! thread. Reads note the subjects and keys they touch, and the writer notes every change made for
//! this thread, so the reconciler can tell which items a change can affect and which evaluations
//! changed anything. See `doc/fleet/smalltalk/idle-cpu-incremental-design`.

use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;

thread_local! {
    static READS: RefCell<Option<BTreeSet<String>>> = const { RefCell::new(None) };
    static WRITES: Cell<u64> = const { Cell::new(0) };
    static DUE: Cell<Option<u128>> = const { Cell::new(None) };
}

/// Note that the work being recorded must run again by `at` (unix ms): a timer it armed, a lease
/// or a time limit it compared against the clock.
pub fn note_due(at: u128) {
    DUE.with(|due| due.set(Some(due.get().map_or(at, |current| current.min(at)))));
}

/// Run `work`, returning the earliest time it noted with [`note_due`]. Nests like [`record`].
pub fn record_due<T>(work: impl FnOnce() -> T) -> (T, Option<u128>) {
    let outer = DUE.with(|due| due.replace(None));
    let result = work();
    let inner = DUE.with(|due| due.replace(outer));
    if let (Some(at), Some(_)) = (inner, outer) {
        note_due(at);
    }
    (result, inner)
}

/// Note that the work being recorded on this thread read `key`: a subject such as
/// `mission-run/…`, or a key such as `actor:agent/…` or `kind:operational.failure`. Free when
/// nothing is recording.
pub fn note_read(key: impl FnOnce() -> String) {
    READS.with(|reads| {
        if let Some(set) = reads.borrow_mut().as_mut() {
            set.insert(key());
        }
    });
}

/// Run `work`, returning what it read. Recording nests: an enclosing recording also gets these
/// reads, even when `work` panics.
pub fn record<T>(work: impl FnOnce() -> T) -> (T, BTreeSet<String>) {
    /// Puts the enclosing recording back, with this one's reads added to it.
    struct Scope(Option<Option<BTreeSet<String>>>);
    impl Drop for Scope {
        fn drop(&mut self) {
            let Some(outer) = self.0.take() else { return };
            READS.with(|reads| {
                let inner = reads.borrow_mut().take().unwrap_or_default();
                *reads.borrow_mut() = outer.map(|mut outer| {
                    outer.extend(inner);
                    outer
                });
            });
        }
    }
    let scope = Scope(Some(
        READS.with(|reads| reads.borrow_mut().replace(BTreeSet::new())),
    ));
    let result = work();
    let reads = READS.with(|reads| reads.borrow().clone().unwrap_or_default());
    drop(scope);
    (result, reads)
}

/// Rows this thread's writes have changed since it started; compare two readings to tell whether
/// the work between them wrote anything.
pub fn writes() -> u64 {
    WRITES.with(Cell::get)
}

pub(crate) fn note_writes(rows: u64) {
    if rows > 0 {
        WRITES.with(|writes| writes.set(writes.get().saturating_add(rows)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recordings_nest_and_cost_nothing_when_off() {
        note_read(|| unreachable!("nothing records"));
        let (inner, outer) = record(|| {
            note_read(|| "a".into());
            let ((), inner) = record(|| note_read(|| "b".into()));
            inner
        });
        assert_eq!(inner, BTreeSet::from(["b".to_owned()]));
        assert_eq!(outer, BTreeSet::from(["a".to_owned(), "b".to_owned()]));
        note_read(|| unreachable!("recording ended"));
    }
}
