//! One ordered line of entries that a mission run works through front first.
//!
//! A mission declares a lane; each run owns the subject `lane/RUN/NAME`. Every
//! change is one claim on that subject: `lane.joined`, `lane.left`,
//! `lane.moved`, `lane.marked`, and `lane.approved`. `replay` rebuilds the lane
//! from those claims in graph time; claims with the same time keep the order the
//! store lists them in, which is the same on every replica (writer, then the
//! writer's own sequence). So every replica that holds the same claims shows the
//! same lane. st keeps the
//! order and the statuses; the run's own work decides what an entry needs and
//! records it here.

use serde::{Deserialize, Serialize};

pub use crate::seat_queue::Placement;

pub const JOINED_CLAIM: &str = "lane.joined";
pub const LEFT_CLAIM: &str = "lane.left";
pub const MOVED_CLAIM: &str = "lane.moved";
pub const MARKED_CLAIM: &str = "lane.marked";
pub const APPROVED_CLAIM: &str = "lane.approved";

pub const CLAIM_KINDS: &[&str] = &[
    JOINED_CLAIM,
    LEFT_CLAIM,
    MOVED_CLAIM,
    MARKED_CLAIM,
    APPROVED_CLAIM,
];

pub const STATES: &[&str] = &["waiting", "held", "ready", "running"];
pub const OUTCOMES: &[&str] = &["completed", "removed"];

/// What one lane claim changes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    Joined,
    Left {
        outcome: String,
    },
    Moved {
        placement: Placement,
        anchor: Option<String>,
    },
    Marked {
        state: String,
        detail: Option<String>,
        head: Option<String>,
    },
    Approved,
}

impl Change {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Joined => "joined",
            Self::Left { .. } => "left",
            Self::Moved { .. } => "moved",
            Self::Marked { .. } => "marked",
            Self::Approved => "approved",
        }
    }
}

/// One lane claim, as the replay reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Event {
    pub claim_id: String,
    pub at_unix_ms: u128,
    pub actor: String,
    pub entry: String,
    pub reason: Option<String>,
    pub change: Change,
}

/// One entry in the lane now.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct Entry {
    pub position: usize,
    pub entry: String,
    /// `waiting` until the run records another status.
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub marked_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub marked_at_unix_ms: Option<u128>,
    pub joined_by: String,
    pub joined_at_unix_ms: u128,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub join_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved_at_unix_ms: Option<u128>,
}

/// A join, leave, move, or approval, as the lane's recent history shows it.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Recent {
    pub kind: String,
    pub entry: String,
    pub actor: String,
    pub at_unix_ms: u128,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Replayed {
    pub entries: Vec<Entry>,
    /// Joins, leaves, moves, and approvals, newest first. Marks are left out:
    /// each entry carries its latest one.
    pub recent: Vec<Recent>,
}

/// Rebuild a lane from its claims, given in the store's replica-stable order.
///
/// Claims apply in graph time; claims with the same time keep their given
/// order. A join
/// adds an entry at the back unless it is already in the lane. A leave removes
/// it, and a later join starts a new membership without the old status or
/// approval. A move, mark, or approval that names an entry which is not in the
/// lane at that point changes nothing, and neither does a move whose anchor is
/// missing.
pub fn replay(events: &[Event], recent_limit: usize) -> Replayed {
    let mut ordered = events.iter().collect::<Vec<_>>();
    ordered.sort_by_key(|event| event.at_unix_ms);
    let mut entries = Vec::<Entry>::new();
    let mut recent = Vec::<Recent>::new();
    for event in ordered {
        let at = entries.iter().position(|entry| entry.entry == event.entry);
        let applied = match (&event.change, at) {
            (Change::Joined, None) => {
                entries.push(Entry {
                    entry: event.entry.clone(),
                    state: "waiting".into(),
                    joined_by: event.actor.clone(),
                    joined_at_unix_ms: event.at_unix_ms,
                    join_reason: event.reason.clone(),
                    ..Entry::default()
                });
                true
            }
            (Change::Joined, Some(_)) => false,
            (Change::Left { .. }, Some(index)) => {
                entries.remove(index);
                true
            }
            (Change::Moved { placement, anchor }, Some(index)) => {
                move_entry(&mut entries, index, *placement, anchor.as_deref())
            }
            (
                Change::Marked {
                    state,
                    detail,
                    head,
                },
                Some(index),
            ) => {
                let entry = &mut entries[index];
                entry.state = state.clone();
                entry.detail = detail.clone();
                entry.head = head.clone();
                entry.marked_by = Some(event.actor.clone());
                entry.marked_at_unix_ms = Some(event.at_unix_ms);
                true
            }
            (Change::Approved, Some(index)) => {
                let entry = &mut entries[index];
                entry.approved_by = Some(event.actor.clone());
                entry.approved_at_unix_ms = Some(event.at_unix_ms);
                true
            }
            (_, None) => false,
        };
        if applied && !matches!(event.change, Change::Marked { .. }) {
            recent.push(recent_record(event));
        }
    }
    for (index, entry) in entries.iter_mut().enumerate() {
        entry.position = index + 1;
    }
    recent.reverse();
    recent.truncate(recent_limit);
    Replayed { entries, recent }
}

fn move_entry(
    entries: &mut Vec<Entry>,
    from: usize,
    placement: Placement,
    anchor: Option<&str>,
) -> bool {
    let anchor = if placement.needs_anchor() {
        match anchor {
            Some(anchor)
                if anchor != entries[from].entry
                    && entries.iter().any(|entry| entry.entry == anchor) =>
            {
                Some(anchor)
            }
            _ => return false,
        }
    } else {
        None
    };
    let moved = entries.remove(from);
    let anchor_at = |entries: &[Entry], anchor: &str| {
        entries
            .iter()
            .position(|entry| entry.entry == anchor)
            .expect("the anchor is in the lane")
    };
    let to = match (placement, anchor) {
        (Placement::Top, _) => 0,
        (Placement::Bottom, _) => entries.len(),
        (Placement::Before, Some(anchor)) => anchor_at(entries, anchor),
        (Placement::After, Some(anchor)) => anchor_at(entries, anchor) + 1,
        (Placement::Before | Placement::After, None) => unreachable!("an anchor was checked"),
    };
    entries.insert(to, moved);
    true
}

fn recent_record(event: &Event) -> Recent {
    let (outcome, placement, anchor) = match &event.change {
        Change::Left { outcome } => (Some(outcome.clone()), None, None),
        Change::Moved { placement, anchor } => {
            (None, Some(placement.as_str().to_owned()), anchor.clone())
        }
        _ => (None, None, None),
    };
    Recent {
        kind: event.change.kind().into(),
        entry: event.entry.clone(),
        actor: event.actor.clone(),
        at_unix_ms: event.at_unix_ms,
        outcome,
        placement,
        anchor,
        reason: event.reason.clone(),
    }
}

/// The subject of lane `name` in `run`.
pub fn subject(run: &str, name: &str) -> String {
    format!(
        "lane/{}/{name}",
        run.strip_prefix("mission-run/").unwrap_or(run)
    )
}

/// The full entry subject for an argument: a subject as given, or the part
/// after the lane's `entries` prefix, with a leading `#` dropped.
pub fn entry_subject(prefix: Option<&str>, argument: &str) -> String {
    let argument = argument.trim();
    let short = argument.strip_prefix('#').unwrap_or(argument);
    match prefix {
        Some(prefix) if !short.is_empty() && !short.starts_with(prefix) && !short.contains('/') => {
            format!("{prefix}{short}")
        }
        _ => argument.to_owned(),
    }
}

/// The short form of an entry for display: the part after the prefix.
pub fn short_entry<'a>(prefix: Option<&str>, entry: &'a str) -> &'a str {
    prefix
        .and_then(|prefix| entry.strip_prefix(prefix))
        .filter(|rest| !rest.is_empty())
        .unwrap_or(entry)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(id: &str, at: u128, entry: &str, change: Change) -> Event {
        Event {
            claim_id: id.into(),
            at_unix_ms: at,
            actor: "agent/driver".into(),
            entry: entry.into(),
            reason: None,
            change,
        }
    }

    fn joined(id: &str, at: u128, entry: &str) -> Event {
        event(id, at, entry, Change::Joined)
    }

    fn moved(id: &str, at: u128, entry: &str, placement: Placement, anchor: Option<&str>) -> Event {
        event(
            id,
            at,
            entry,
            Change::Moved {
                placement,
                anchor: anchor.map(str::to_owned),
            },
        )
    }

    fn marked(id: &str, at: u128, entry: &str, state: &str) -> Event {
        event(
            id,
            at,
            entry,
            Change::Marked {
                state: state.into(),
                detail: Some(format!("{entry} is {state}")),
                head: None,
            },
        )
    }

    fn left(id: &str, at: u128, entry: &str) -> Event {
        event(
            id,
            at,
            entry,
            Change::Left {
                outcome: "completed".into(),
            },
        )
    }

    fn order(replayed: &Replayed) -> Vec<&str> {
        replayed
            .entries
            .iter()
            .map(|entry| entry.entry.as_str())
            .collect()
    }

    #[test]
    fn joins_line_up_in_graph_time_and_a_repeated_join_changes_nothing() {
        let events = [
            joined("c1", 30, "c"),
            joined("a1", 10, "a"),
            joined("b1", 20, "b"),
            joined("a2", 40, "a"),
        ];
        let replayed = replay(&events, 10);
        assert_eq!(order(&replayed), ["a", "b", "c"]);
        assert_eq!(
            replayed
                .entries
                .iter()
                .map(|entry| entry.position)
                .collect::<Vec<_>>(),
            [1, 2, 3]
        );
        assert_eq!(replayed.entries[0].joined_at_unix_ms, 10);
        assert_eq!(replayed.entries[0].state, "waiting");
        assert_eq!(replayed.recent.len(), 3, "the repeated join is not history");
    }

    #[test]
    fn claims_with_the_same_time_keep_the_store_order() {
        let events = [
            joined("z", 10, "first"),
            joined("a", 10, "second"),
            marked("m", 10, "second", "ready"),
        ];
        let replayed = replay(&events, 10);
        assert_eq!(order(&replayed), ["first", "second"]);
        assert_eq!(replayed.entries[1].state, "ready");
    }

    #[test]
    fn each_placement_moves_one_entry() {
        let base = [
            joined("1", 1, "a"),
            joined("2", 2, "b"),
            joined("3", 3, "c"),
            joined("4", 4, "d"),
        ];
        let cases = [
            (
                moved("5", 5, "c", Placement::Top, None),
                ["c", "a", "b", "d"],
            ),
            (
                moved("5", 5, "a", Placement::Bottom, None),
                ["b", "c", "d", "a"],
            ),
            (
                moved("5", 5, "d", Placement::Before, Some("b")),
                ["a", "d", "b", "c"],
            ),
            (
                moved("5", 5, "a", Placement::After, Some("c")),
                ["b", "c", "a", "d"],
            ),
        ];
        for (movement, expected) in cases {
            let mut events = base.to_vec();
            events.push(movement);
            assert_eq!(order(&replay(&events, 10)), expected);
        }
    }

    #[test]
    fn a_move_mark_or_approval_for_a_missing_entry_changes_nothing() {
        let events = [
            joined("1", 1, "a"),
            joined("2", 2, "b"),
            moved("3", 3, "missing", Placement::Top, None),
            moved("4", 4, "b", Placement::Before, Some("missing")),
            moved("5", 5, "b", Placement::After, None),
            moved("6", 6, "b", Placement::Before, Some("b")),
            marked("7", 7, "missing", "ready"),
            event("8", 8, "missing", Change::Approved),
            left("9", 9, "missing"),
        ];
        let replayed = replay(&events, 10);
        assert_eq!(order(&replayed), ["a", "b"]);
        assert_eq!(replayed.recent.len(), 2, "only the two joins applied");
    }

    #[test]
    fn a_rejoin_goes_to_the_back_without_its_old_status_or_approval() {
        let events = [
            joined("1", 1, "a"),
            joined("2", 2, "b"),
            marked("3", 3, "a", "ready"),
            event("4", 4, "a", Change::Approved),
            left("5", 5, "a"),
            marked("6", 6, "a", "running"),
            joined("7", 7, "a"),
        ];
        let replayed = replay(&events, 10);
        assert_eq!(order(&replayed), ["b", "a"]);
        let rejoined = &replayed.entries[1];
        assert_eq!(rejoined.state, "waiting");
        assert_eq!(rejoined.detail, None);
        assert_eq!(rejoined.approved_by, None);
        assert_eq!(rejoined.joined_at_unix_ms, 7);
    }

    #[test]
    fn marks_and_approvals_stay_with_their_entry_and_only_history_is_recent() {
        let mut approval = event("4", 4, "b", Change::Approved);
        approval.actor = "person/ada".into();
        approval.reason = Some("looked at it".into());
        let events = [
            joined("1", 1, "a"),
            joined("2", 2, "b"),
            marked("3", 3, "a", "running"),
            approval,
            moved("5", 5, "a", Placement::Bottom, None),
            marked("6", 6, "a", "ready"),
        ];
        let replayed = replay(&events, 10);
        assert_eq!(order(&replayed), ["b", "a"]);
        assert_eq!(replayed.entries[1].state, "ready");
        assert_eq!(replayed.entries[1].marked_at_unix_ms, Some(6));
        assert_eq!(
            replayed.entries[0].approved_by.as_deref(),
            Some("person/ada")
        );
        assert_eq!(
            replayed
                .recent
                .iter()
                .map(|recent| recent.kind.as_str())
                .collect::<Vec<_>>(),
            ["moved", "approved", "joined", "joined"],
            "newest first, without marks"
        );
        assert_eq!(replayed.recent[0].placement.as_deref(), Some("bottom"));
        assert_eq!(replay(&events, 2).recent.len(), 2);
    }

    #[test]
    fn entry_arguments_expand_after_the_prefix() {
        let prefix = Some("resource/github/acme/app/ci/pull-request/");
        assert_eq!(
            entry_subject(prefix, "42"),
            "resource/github/acme/app/ci/pull-request/42"
        );
        assert_eq!(
            entry_subject(prefix, "#42"),
            "resource/github/acme/app/ci/pull-request/42"
        );
        assert_eq!(
            entry_subject(prefix, "resource/github/acme/app/ci/pull-request/42"),
            "resource/github/acme/app/ci/pull-request/42"
        );
        assert_eq!(entry_subject(None, "resource/x"), "resource/x");
        assert_eq!(
            short_entry(prefix, "resource/github/acme/app/ci/pull-request/42"),
            "42"
        );
        assert_eq!(short_entry(prefix, "resource/other"), "resource/other");
        assert_eq!(
            subject("mission-run/example/train/1", "app"),
            "lane/example/train/1/app"
        );
    }
}
