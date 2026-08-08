//! The restart cap (M1b) — st2's crash-loop guard, driven by the job's declared `restart{}` policy
//! (`docs/vrs/requirements.md` R31), so every conformant runner behaves identically on a flapping
//! task.
//!
//! st2 owns liveness (it respawns a dead task on the next reconcile), so it also owns the restart
//! decision. Per task it enforces `delay` spacing, then applies `mode`: `delay` **rate-limits** using
//! a sliding `interval` window (keep restarting once the window clears), while `fail` **parks** the
//! task (give up + surface it) once its failure budget is spent.
//!
//! Those two need different counters, because a rate window cannot express a budget. When a task
//! stays up longer than roughly `interval / attempts` before dying again, earlier launches can age
//! out before `attempts` coexist in the window, so a window-scoped `fail` budget never exhausts.
//! The `fail` counter instead spans launches until the task *stays up* for a full `interval`;
//! reconcile cadence cannot outrun it. That makes [`FlappingCap::end_pass`] load-bearing: it is how
//! the cap learns a task survived.
//!
//! The clock is injected (`now: Instant`) so every branch is unit-testable without sleeping.

use std::collections::HashMap;
use std::collections::HashSet;
use std::time::Instant;

use agent_spec::spec::{Restart, RestartMode};

/// What to do with a would-be (re)launch this pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartDecision {
    /// Launch it now.
    Allow,
    /// Too soon after the last launch (`delay` spacing) — skip, retry next pass.
    Delaying,
    /// `attempts` used up within `interval` under `mode = delay` — skip until the window clears.
    RateLimited,
    /// `attempts` exhausted under `mode = fail` — parked; give up and surface it.
    GaveUp,
}

/// Per-task launch history + the parked set. One instance lives in the supervisor loop.
#[derive(Debug, Default)]
pub struct FlappingCap {
    launches: HashMap<String, Vec<Instant>>,
    last_launch: HashMap<String, Instant>,
    parked: HashSet<String>,
    /// `mode = fail`'s durable budget: launches since the task last stayed up for a full `interval`.
    /// Deliberately not wall-clock scoped — see [`end_pass`](Self::end_pass).
    consecutive: HashMap<String, u32>,
    /// When a task was first observed to have survived a pass. Cleared the moment it needs
    /// relaunching again — or the moment a pass stops seeing it alive — so only *uninterrupted,
    /// observed* uptime counts toward forgiveness.
    healthy_since: HashMap<String, Instant>,
    /// Ids [`decide`](Self::decide)d this pass — i.e. proved *not* up. Liveness is supplied
    /// separately to [`end_pass`](Self::end_pass); it is never the complement of this set.
    decided_this_pass: HashSet<String>,
}

impl FlappingCap {
    pub fn new() -> Self {
        Self::default()
    }

    /// True once a task has been parked (gave up).
    pub fn is_parked(&self, id: &str) -> bool {
        self.parked.contains(id)
    }

    /// The parked task ids — for reporting "gave up" once each.
    pub fn parked_ids(&self) -> impl Iterator<Item = &String> {
        self.parked.iter()
    }

    /// Decide whether `id` may be (re)launched at `now` under `policy`. On `Allow` the caller should
    /// spawn and then call [`record`](Self::record).
    pub fn decide(&mut self, id: &str, now: Instant, policy: &Restart) -> RestartDecision {
        // Deciding at all means the task needed (re)launching, so it is not up right now.
        self.decided_this_pass.insert(id.to_string());
        let survived = self.healthy_since.remove(id);

        if self.parked.contains(id) {
            return RestartDecision::GaveUp;
        }
        // Staying up for a full `interval` forgives the failure history.
        if survived.is_some_and(|since| now.duration_since(since) >= policy.interval) {
            self.consecutive.remove(id);
        }
        // `delay` spacing between restarts.
        if let Some(&last) = self.last_launch.get(id)
            && now.duration_since(last) < policy.delay
        {
            return RestartDecision::Delaying;
        }
        match policy.mode {
            // A failure budget, not a rate: counted since the task last stayed up, so a reconcile
            // cadence coarser than `interval / attempts` cannot stop it accruing.
            RestartMode::Fail => {
                if self.consecutive.get(id).copied().unwrap_or(0) >= policy.attempts {
                    self.parked.insert(id.to_string());
                    return RestartDecision::GaveUp;
                }
            }
            // A rate limit, exactly as documented: `attempts` within the sliding `interval` window.
            RestartMode::Delay => {
                if self.recent_count(id, now, policy) >= policy.attempts as usize {
                    return RestartDecision::RateLimited;
                }
            }
        }
        RestartDecision::Allow
    }

    /// Record a launch of `id` at `now` (call only after a successful spawn).
    pub fn record(&mut self, id: &str, now: Instant) {
        self.launches.entry(id.to_string()).or_default().push(now);
        self.last_launch.insert(id.to_string(), now);
        *self.consecutive.entry(id.to_string()).or_default() += 1;
    }

    /// Close a reconcile pass at `now`, given the ids the pass PROVED alive. Uptime is what forgives
    /// a failure history, so the budget is tied to the task actually recovering rather than to
    /// wall-clock elapsing while it keeps dying.
    ///
    /// Liveness must be *observed*, never inferred from a task's absence from the pass: a task can
    /// go unconsidered (owner failed to materialize, launch gated on hooks, death debounced as a
    /// flicker) while it is in fact dead. So a tracked task this pass did not observe alive has its
    /// accrued uptime dropped, not extended — otherwise a `healthy_since` from *before* the blind
    /// spot survives it, and the next [`decide`](Self::decide) measures uptime across a stretch
    /// nobody looked at and hands back a full budget.
    pub fn end_pass(&mut self, now: Instant, observed_live: &[String]) {
        let decided = std::mem::take(&mut self.decided_this_pass);
        let live: HashSet<&str> = observed_live.iter().map(String::as_str).collect();
        let tracked: Vec<String> = self.consecutive.keys().cloned().collect();
        for id in tracked {
            if !decided.contains(&id) && live.contains(id.as_str()) {
                self.healthy_since.entry(id).or_insert(now);
            } else {
                self.healthy_since.remove(&id);
            }
        }
    }

    /// Count (and prune) launches of `id` within `policy.interval` ending at `now`.
    fn recent_count(&mut self, id: &str, now: Instant, policy: &Restart) -> usize {
        let Some(times) = self.launches.get_mut(id) else {
            return 0;
        };
        let window = policy.interval;
        times.retain(|t| now.duration_since(*t) < window);
        times.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn policy(attempts: u32, interval_s: u64, delay_s: u64, mode: RestartMode) -> Restart {
        Restart {
            attempts,
            interval: Duration::from_secs(interval_s),
            delay: Duration::from_secs(delay_s),
            mode,
        }
    }

    #[test]
    fn fail_mode_parks_after_attempts() {
        let mut cap = FlappingCap::new();
        let p = policy(3, 60, 0, RestartMode::Fail);
        let t0 = Instant::now();
        for i in 0..3 {
            let now = t0 + Duration::from_secs(i);
            assert_eq!(cap.decide("p", now, &p), RestartDecision::Allow);
            cap.record("p", now);
        }
        assert_eq!(cap.decide("p", t0 + Duration::from_secs(4), &p), RestartDecision::GaveUp);
        assert!(cap.is_parked("p"));
        // stays parked even after the window empties
        assert_eq!(cap.decide("p", t0 + Duration::from_secs(600), &p), RestartDecision::GaveUp);
    }

    #[test]
    fn delay_mode_rate_limits_but_never_parks() {
        let mut cap = FlappingCap::new();
        let p = policy(3, 60, 0, RestartMode::Delay);
        let t0 = Instant::now();
        for i in 0..3 {
            let now = t0 + Duration::from_secs(i);
            cap.decide("p", now, &p);
            cap.record("p", now);
        }
        // 4th within the window → rate-limited, NOT parked.
        assert_eq!(cap.decide("p", t0 + Duration::from_secs(4), &p), RestartDecision::RateLimited);
        assert!(!cap.is_parked("p"));
        // Once the window clears, it's allowed again.
        assert_eq!(cap.decide("p", t0 + Duration::from_secs(120), &p), RestartDecision::Allow);
    }

    #[test]
    fn delay_spacing_is_enforced() {
        let mut cap = FlappingCap::new();
        let p = policy(5, 60, 10, RestartMode::Delay);
        let t0 = Instant::now();
        cap.record("p", t0);
        // 5s later — still within the 10s delay.
        assert_eq!(cap.decide("p", t0 + Duration::from_secs(5), &p), RestartDecision::Delaying);
        // 11s later — delay satisfied.
        assert_eq!(cap.decide("p", t0 + Duration::from_secs(11), &p), RestartDecision::Allow);
    }

    #[test]
    fn parking_is_per_task() {
        let mut cap = FlappingCap::new();
        let p = policy(1, 60, 0, RestartMode::Fail);
        let t0 = Instant::now();
        cap.record("a", t0);
        assert_eq!(cap.decide("a", t0 + Duration::from_secs(1), &p), RestartDecision::GaveUp);
        assert_eq!(cap.decide("b", t0 + Duration::from_secs(1), &p), RestartDecision::Allow);
    }

    /// Reproduction for the crash-loop budget being unreachable at the supervisor's own cadence.
    ///
    /// The window counts only launches newer than `interval`, so at a reconcile cadence `c` the count
    /// seen at decision time saturates at `ceil(interval / c) - 1`. `st2 up --interval` defaults to
    /// 30s and the default policy is `attempts 3 / interval 60s`, giving a ceiling of 1: the budget
    /// never reaches 3, `mode = fail` never engages, and a task that dies every pass relaunches
    /// forever.
    ///
    /// The launch rate is the thing that is bounded. A supervisor reconciling every 30s can start a
    /// task at most twice a minute, which is below the 3-per-minute the policy budgets, so no
    /// windowing scheme can let the budget accrue — the counter has to stop being wall-clock scoped.
    ///
    /// Positive control: `fail_mode_parks_after_attempts` drives this exact policy at 1s spacing and
    /// does reach `GaveUp`, so a failure here is the cadence and not the harness.
    #[test]
    fn fail_mode_parks_at_the_default_supervisor_cadence() {
        let mut cap = FlappingCap::new();
        let p = policy(3, 60, 0, RestartMode::Fail);
        let t0 = Instant::now();
        let cadence = Duration::from_secs(30);

        // The task dies before every pass, so every pass is a relaunch attempt.
        let mut launches = 0u32;
        for pass in 0..20u32 {
            let now = t0 + cadence * pass;
            match cap.decide("p", now, &p) {
                RestartDecision::Allow => {
                    cap.record("p", now);
                    launches += 1;
                }
                RestartDecision::GaveUp => {
                    assert_eq!(launches, 3, "parked, but not after exactly `attempts` launches");
                    return;
                }
                other => panic!("unexpected {other:?} at pass {pass} (delay is 0)"),
            }
        }
        panic!(
            "never parked: {launches} launches over 20 passes at a {}s cadence under `attempts 3`",
            cadence.as_secs()
        );
    }

    /// The budget is spent by failing, not by existing: a task that comes back and stays up for a
    /// full `interval` is forgiven and may use the budget again later.
    #[test]
    fn staying_up_for_an_interval_forgives_the_budget() {
        let mut cap = FlappingCap::new();
        let p = policy(3, 60, 0, RestartMode::Fail);
        let t0 = Instant::now();

        // Two failures, then it comes back up. A pass that had to relaunch it saw it dead.
        for i in 0..2 {
            let now = t0 + Duration::from_secs(30 * i);
            assert_eq!(cap.decide("p", now, &p), RestartDecision::Allow);
            cap.record("p", now);
            cap.end_pass(now, &[]);
        }
        // Passes 2..6 observe it alive.
        for i in 2..6 {
            cap.end_pass(t0 + Duration::from_secs(30 * i), &["p".to_string()]);
        }
        // It dies again 120s after recovering: history forgiven, full budget available.
        let mut launches = 0;
        for i in 6..12 {
            let now = t0 + Duration::from_secs(30 * i);
            match cap.decide("p", now, &p) {
                RestartDecision::Allow => {
                    cap.record("p", now);
                    cap.end_pass(now, &[]);
                    launches += 1;
                }
                RestartDecision::GaveUp => {
                    assert_eq!(launches, 3, "forgiveness did not restore the full budget");
                    return;
                }
                other => panic!("unexpected {other:?}"),
            }
        }
        panic!("never parked after forgiveness: {launches} launches");
    }

    /// Only *uninterrupted* uptime forgives. A task that survives a pass but dies again well inside
    /// `interval` is still crash-looping, and must still reach `GaveUp`.
    #[test]
    fn a_slow_flapper_still_parks() {
        let mut cap = FlappingCap::new();
        let p = policy(3, 60, 0, RestartMode::Fail);
        let t0 = Instant::now();
        let cadence = Duration::from_secs(30);

        // Alternating: up for one pass, dead the next. Never a full 60s of uptime.
        let mut launches = 0;
        for pass in 0..40u32 {
            let now = t0 + cadence * pass;
            let mut observed_live: &[String] = &[];
            let alive = ["p".to_string()];
            if pass % 2 == 0 {
                match cap.decide("p", now, &p) {
                    RestartDecision::Allow => {
                        cap.record("p", now);
                        launches += 1;
                    }
                    RestartDecision::GaveUp => {
                        assert_eq!(launches, 3);
                        return;
                    }
                    other => panic!("unexpected {other:?}"),
                }
            } else {
                observed_live = &alive;
            }
            cap.end_pass(now, observed_live);
        }
        panic!("slow flapper never parked: {launches} launches over 40 passes");
    }

    /// A pass that does not observe a tracked task alive must drop its accrued uptime, not leave it
    /// standing. This is the defense itself: without the `else` arm the `healthy_since` recorded by
    /// an earlier pass outlives the blind spot, and the next `decide` measures uptime across a
    /// stretch nobody looked at and hands the task a full budget back.
    ///
    /// `interval` is 0 deliberately. It is the most sensitive setting, not an obstacle: any surviving
    /// `healthy_since` then satisfies the forgiveness check outright, so a stale entry forgives and a
    /// dropped one cannot. The alive pass comes *after* a launching pass because `end_pass` only
    /// walks `consecutive`, which `record` populates — before the first launch the task is untracked
    /// and the omission would have nothing to leave stale.
    #[test]
    fn a_pass_that_does_not_observe_a_task_drops_its_uptime() {
        let mut cap = FlappingCap::new();
        let p = policy(3, 0, 0, RestartMode::Fail);
        let t0 = Instant::now();
        let alive = ["p".to_string()];

        // 1. Dead: launch it, so it is tracked.
        assert_eq!(cap.decide("p", t0, &p), RestartDecision::Allow);
        cap.record("p", t0);
        cap.end_pass(t0, &[]);

        // 2. Observed alive: uptime starts accruing.
        cap.end_pass(t0 + Duration::from_secs(1), &alive);

        // 3. THE OMISSION: the pass ran but never considered this task — not decided, and not proved
        //    alive. Its accrued uptime must not survive a pass that did not look at it.
        cap.end_pass(t0 + Duration::from_secs(2), &[]);

        // 4-5. Dead again, twice. Uptime dropped in 3, so nothing forgives the budget here.
        for i in 3..5 {
            let now = t0 + Duration::from_secs(i);
            assert_eq!(
                cap.decide("p", now, &p),
                RestartDecision::Allow,
                "pass {i} should still be inside the budget"
            );
            cap.record("p", now);
            cap.end_pass(now, &[]);
        }

        // Three launches consumed, so `attempts` is spent. If the omission had left `healthy_since`
        // standing, pass 4 would have been forgiven and this would still be `Allow`.
        assert_eq!(
            cap.decide("p", t0 + Duration::from_secs(5), &p),
            RestartDecision::GaveUp,
            "uptime survived a pass that never observed the task, so the budget was forgiven"
        );
    }
}
