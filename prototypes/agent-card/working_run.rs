//! Source-only per-incarnation harness-state aggregate prototype.
//!
//! A balanced canonical event tree stores this summary at each node. Point
//! insert/delete/reorder recomputes only its ancestor path. The root returns
//! the first `working` observation after the last different state, matching
//! `Store::agent_working_since` without folding the incarnation each read.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct WorkingRun {
    empty: bool,
    all_working: bool,
    first_working_ms: Option<u128>,
    suffix_working_start_ms: Option<u128>,
}

impl WorkingRun {
    const fn empty() -> Self {
        Self {
            empty: true,
            all_working: true,
            first_working_ms: None,
            suffix_working_start_ms: None,
        }
    }

    fn event(state: &str, accepted_at_ms: u128) -> Self {
        let working = state == "working";
        Self {
            empty: false,
            all_working: working,
            first_working_ms: working.then_some(accepted_at_ms),
            suffix_working_start_ms: working.then_some(accepted_at_ms),
        }
    }

    fn then(self, next: Self) -> Self {
        if self.empty {
            return next;
        }
        if next.empty {
            return self;
        }
        Self {
            empty: false,
            all_working: self.all_working && next.all_working,
            first_working_ms: self.first_working_ms.or(next.first_working_ms),
            suffix_working_start_ms: if next.all_working {
                self.suffix_working_start_ms.or(next.first_working_ms)
            } else {
                next.suffix_working_start_ms
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::WorkingRun;

    fn fold(events: &[(&str, u128)]) -> WorkingRun {
        events.iter().fold(WorkingRun::empty(), |summary, (state, at)| {
            summary.then(WorkingRun::event(state, *at))
        })
    }

    #[test]
    fn suffix_start_matches_the_old_fold_after_reorder_and_delete() {
        let events = [("idle", 1), ("working", 2), ("working", 3), ("idle", 4), ("working", 5)];
        assert_eq!(fold(&events).suffix_working_start_ms, Some(5));
        let repaired = [("idle", 1), ("working", 2), ("working", 3), ("working", 5)];
        assert_eq!(fold(&repaired).suffix_working_start_ms, Some(2));
        assert_eq!(fold(&repaired[..3]).suffix_working_start_ms, Some(2));
    }

    #[test]
    fn combination_is_associative_across_tree_shapes() {
        let a = WorkingRun::event("working", 1);
        let b = WorkingRun::event("idle", 2);
        let c = WorkingRun::event("working", 3);
        assert_eq!(a.then(b).then(c), a.then(b.then(c)));
        assert_eq!(a.then(b).then(c).suffix_working_start_ms, Some(3));
        assert_eq!(WorkingRun::empty().then(a), a);
        assert_eq!(a.then(WorkingRun::empty()), a);
    }
}
