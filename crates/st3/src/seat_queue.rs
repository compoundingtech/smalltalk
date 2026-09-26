//! The ordered queue of mission runs for one agent seat.
//!
//! A seat holds at most one claim. A run joins the end of the seat's queue when
//! it first has a step assigned to that seat and leaves when the run is
//! terminal. Each `agent.queue.moved` claim moves one run and is replayed after
//! the joins recorded before it, so a move is one durable record with its own
//! actor and time. Order inside a run still comes from the mission's
//! dependencies; the seat queue only orders runs against each other.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::model::StepRunView;

pub const MOVED_CLAIM: &str = "agent.queue.moved";

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Placement {
    Top,
    Bottom,
    Before,
    After,
}

impl Placement {
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "top" => Self::Top,
            "bottom" => Self::Bottom,
            "before" => Self::Before,
            "after" => Self::After,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Top => "top",
            Self::Bottom => "bottom",
            Self::Before => "before",
            Self::After => "after",
        }
    }

    pub fn needs_anchor(self) -> bool {
        matches!(self, Self::Before | Self::After)
    }
}

/// The first graph time a run had a step for the seat.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueueJoin {
    pub run: String,
    pub at_unix_ms: u128,
}

/// One recorded move, in graph order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueueMove {
    pub run: String,
    pub placement: Placement,
    pub anchor: Option<String>,
    pub at_unix_ms: u128,
}

/// Rebuild a seat's run order from its joins and its moves.
///
/// Joins apply in time order with the run as a stable tie breaker. Each move
/// applies after every join recorded at or before it. A move that names a run
/// whose join carries a later timestamp (a writer clock ahead of the mover's)
/// joins that run first, because the mover could only name a queued run. A
/// move that names a run with no join is ignored.
pub fn replay(joins: &[QueueJoin], moves: &[QueueMove]) -> Vec<String> {
    let mut joins = joins.to_vec();
    joins.sort_by(|left, right| {
        left.at_unix_ms
            .cmp(&right.at_unix_ms)
            .then_with(|| left.run.cmp(&right.run))
    });
    let known = joins
        .iter()
        .map(|join| join.run.as_str())
        .collect::<BTreeSet<_>>();
    let mut order = Vec::<String>::with_capacity(joins.len());
    let mut pending = joins.iter().peekable();
    for movement in moves {
        while let Some(join) = pending.next_if(|join| join.at_unix_ms <= movement.at_unix_ms) {
            join_run(&mut order, &join.run);
        }
        let named = std::iter::once(movement.run.as_str()).chain(movement.anchor.as_deref());
        for run in named {
            if known.contains(run) {
                join_run(&mut order, run);
            }
        }
        apply_move(&mut order, movement);
    }
    for join in pending {
        join_run(&mut order, &join.run);
    }
    order
}

fn join_run(order: &mut Vec<String>, run: &str) {
    if !order.iter().any(|queued| queued == run) {
        order.push(run.to_owned());
    }
}

fn apply_move(order: &mut Vec<String>, movement: &QueueMove) {
    let Some(from) = order.iter().position(|run| *run == movement.run) else {
        return;
    };
    let anchor = if movement.placement.needs_anchor() {
        match movement.anchor.as_deref() {
            Some(anchor) if anchor != movement.run && order.iter().any(|run| run == anchor) => {
                Some(anchor)
            }
            _ => return,
        }
    } else {
        None
    };
    let run = order.remove(from);
    let to = match (movement.placement, anchor) {
        (Placement::Top, _) => 0,
        (Placement::Bottom, _) => order.len(),
        (Placement::Before, Some(anchor)) => order
            .iter()
            .position(|queued| queued == anchor)
            .expect("the anchor is queued"),
        (Placement::After, Some(anchor)) => {
            order
                .iter()
                .position(|queued| queued == anchor)
                .expect("the anchor is queued")
                + 1
        }
        (Placement::Before | Placement::After, None) => unreachable!("an anchor was checked"),
    };
    order.insert(to, run);
}

/// A current step as the seat selector sees it.
#[derive(Clone, Copy, Debug)]
pub struct SeatStep<'a> {
    pub subject: &'a str,
    pub run: &'a str,
    pub step: &'a str,
    pub status: &'a str,
    pub assignee: Option<&'a str>,
    pub claimant: Option<&'a str>,
    pub available_to: &'a [String],
    pub created_at_unix_ms: u128,
}

impl<'a> From<&'a StepRunView> for SeatStep<'a> {
    fn from(view: &'a StepRunView) -> Self {
        Self {
            subject: &view.subject,
            run: &view.run,
            step: &view.step,
            status: &view.status,
            assignee: view.assigned_to.as_deref(),
            claimant: view.claimant.as_deref(),
            available_to: &view.available_to,
            created_at_unix_ms: view.created_at_unix_ms,
        }
    }
}

/// What one seat holds and what it takes next.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SeatSelection<'a> {
    /// Claimed, working, or verifying steps that occupy the seat.
    pub held: Vec<&'a str>,
    /// Ready steps assigned to the seat, in seat-queue order.
    pub ready: Vec<&'a str>,
}

impl<'a> SeatSelection<'a> {
    /// The seat's next work, whether or not it is free now.
    pub fn next(&self) -> Option<&'a str> {
        self.ready.first().copied()
    }

    /// The step to wake: the next work, only while the seat holds nothing.
    pub fn wake(&self) -> Option<&'a str> {
        if self.held.is_empty() {
            self.next()
        } else {
            None
        }
    }
}

/// The one next-work selector for an agent seat.
///
/// Ready steps sort by their run's queue position, then by creation time and
/// subject inside the run. A run without a ready step for the seat is passed
/// over, so a run waiting on a gate or another seat never blocks the runs after
/// it, and it becomes next again as soon as it has ready work. A nested step
/// whose listed parent step has the same selector is reached through that
/// parent and is not selected separately. Reordering never releases a held
/// step: `held` comes only from claims.
pub fn select<'a>(agent: &str, steps: &[SeatStep<'a>], run_order: &[String]) -> SeatSelection<'a> {
    let held = steps
        .iter()
        .filter(|step| {
            matches!(step.status, "claimed" | "working" | "verifying")
                && (step.claimant == Some(agent) || step.assignee == Some(agent))
        })
        .map(|step| step.subject)
        .collect();
    let rank = |run: &str| {
        run_order
            .iter()
            .position(|queued| queued == run)
            .unwrap_or(usize::MAX)
    };
    let mut ready = steps
        .iter()
        .filter(|step| {
            step.status == "ready"
                && step.assignee == Some(agent)
                && !reached_through_parent(step, steps)
        })
        .collect::<Vec<_>>();
    ready.sort_by(|left, right| {
        rank(left.run)
            .cmp(&rank(right.run))
            .then_with(|| left.created_at_unix_ms.cmp(&right.created_at_unix_ms))
            .then_with(|| left.subject.cmp(right.subject))
    });
    SeatSelection {
        held,
        ready: ready.into_iter().map(|step| step.subject).collect(),
    }
}

/// True when a listed ancestor step in the same run has the same selector.
pub fn reached_through_parent(step: &SeatStep<'_>, steps: &[SeatStep<'_>]) -> bool {
    steps.iter().any(|candidate| {
        candidate.run == step.run
            && candidate.assignee == step.assignee
            && candidate.available_to == step.available_to
            && candidate.step.len() < step.step.len()
            && step.step.starts_with(candidate.step)
            && step.step.as_bytes().get(candidate.step.len()) == Some(&b'/')
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn join(run: &str, at: u128) -> QueueJoin {
        QueueJoin {
            run: run.into(),
            at_unix_ms: at,
        }
    }

    fn moved(run: &str, placement: Placement, anchor: Option<&str>, at: u128) -> QueueMove {
        QueueMove {
            run: run.into(),
            placement,
            anchor: anchor.map(str::to_owned),
            at_unix_ms: at,
        }
    }

    #[test]
    fn joins_keep_start_order_and_later_runs_join_the_end() {
        let joins = [join("c", 30), join("a", 10), join("b", 20)];
        assert_eq!(replay(&joins, &[]), ["a", "b", "c"]);
        let moves = [moved("a", Placement::Bottom, None, 25)];
        assert_eq!(
            replay(&joins, &moves),
            ["b", "a", "c"],
            "a run that joins after a move still joins the end"
        );
    }

    #[test]
    fn each_placement_moves_one_run() {
        let joins = [join("a", 1), join("b", 2), join("c", 3), join("d", 4)];
        let cases = [
            (moved("c", Placement::Top, None, 5), ["c", "a", "b", "d"]),
            (moved("a", Placement::Bottom, None, 5), ["b", "c", "d", "a"]),
            (
                moved("d", Placement::Before, Some("b"), 5),
                ["a", "d", "b", "c"],
            ),
            (
                moved("a", Placement::After, Some("c"), 5),
                ["b", "c", "a", "d"],
            ),
        ];
        for (movement, expected) in cases {
            assert_eq!(replay(&joins, &[movement]), expected);
        }
    }

    #[test]
    fn a_move_joins_named_runs_whose_writer_clock_ran_ahead() {
        let joins = [join("a", 10), join("b", 50)];
        let moves = [moved("b", Placement::Top, None, 20)];
        assert_eq!(replay(&joins, &moves), ["b", "a"]);
    }

    #[test]
    fn unusable_moves_are_ignored() {
        let joins = [join("a", 1), join("b", 2)];
        let moves = [
            moved("missing", Placement::Top, None, 3),
            moved("b", Placement::Before, Some("missing"), 4),
            moved("b", Placement::After, None, 5),
            moved("b", Placement::Before, Some("b"), 6),
        ];
        assert_eq!(replay(&joins, &moves), ["a", "b"]);
    }

    #[test]
    fn nested_steps_are_reached_through_their_listed_parent_only() {
        let none = Vec::new();
        let step = |subject: &'static str, path: &'static str| SeatStep {
            subject,
            run: "mission-run/one",
            step: path,
            status: "ready",
            assignee: Some("agent/worker"),
            claimant: None,
            available_to: &none,
            created_at_unix_ms: 1,
        };
        let steps = [
            step("step-run/one/build", "build"),
            step("step-run/one/build/check", "build/check"),
            step("step-run/one/builder", "builder"),
        ];
        assert!(!reached_through_parent(&steps[0], &steps));
        assert!(reached_through_parent(&steps[1], &steps));
        assert!(
            !reached_through_parent(&steps[2], &steps),
            "a sibling that shares a name prefix is not nested"
        );
        let selection = select("agent/worker", &steps, &["mission-run/one".into()]);
        assert_eq!(
            selection.ready,
            ["step-run/one/build", "step-run/one/builder"]
        );
    }
}
