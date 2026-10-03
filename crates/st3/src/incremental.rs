//! Which items a reconcile pass must evaluate.
//!
//! Each item (a mission run, later members and intake declarations) remembers what its last
//! evaluation read, and when its result could next change with time alone. A pass reads what
//! changed since the last one, turns each change into the keys a read would have noted, and marks
//! the items that read any of them. An item needs evaluating when it is new, marked, or due. See
//! `doc/fleet/smalltalk/idle-cpu-incremental-design`.
//!
//! For now every item is still evaluated on every pass, and an evaluation that wrote although its
//! item did not need evaluating is counted as a correction: a write an incremental pass would
//! have missed.

use std::collections::{BTreeSet, HashMap};
use std::sync::Mutex;

use anyhow::Result;
use serde_json::Value;

use crate::store::{Change, Store};

#[derive(Default)]
pub struct Incremental {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// The change feed's watermark; `None` until the first pass, which evaluates every item.
    watermark: Option<(u64, i64)>,
    items: HashMap<String, Item>,
    /// Which items read each key.
    readers: HashMap<String, BTreeSet<String>>,
    /// The last status seen for each exec runtime an item waits on (`exec:` keys).
    execs: HashMap<String, Option<String>>,
}

struct Item {
    reads: BTreeSet<String>,
    due: Option<u128>,
    dirty: bool,
}

/// The keys a change can affect: its subject, its actor's work, its kind, and for some kinds a
/// key a read notes for subjects it cannot name in advance.
pub fn change_keys(change: &Change) -> Vec<String> {
    let mut keys = vec![change.subject.clone(), format!("kind:{}", change.kind)];
    if let Some(actor) = &change.actor {
        keys.push(format!("actor:{actor}"));
    }
    let field = |name: &str| -> Option<String> {
        let body: Value = serde_json::from_str(&change.body).ok()?;
        body.get("fields")
            .unwrap_or(&body)
            .get(name)
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    match change.kind.as_str() {
        "message.sent" => keys.extend(field("to").map(|to| format!("mailbox:{to}"))),
        "mission-run.created" => {
            keys.extend(field("root_mission_run").map(|root| format!("children:{root}")));
            keys.extend(field("parent_step_run").map(|step| format!("children-of-step:{step}")));
        }
        "intent.desired" => {
            keys.extend(field("owner_run").map(|run| format!("owned:{run}")));
            keys.extend(field("owner_step").map(|step| format!("owned-step:{step}")));
        }
        "run-generation.created" => {
            keys.extend(field("run").map(|run| format!("generations:{run}")));
        }
        _ => {}
    }
    keys
}

impl Incremental {
    /// Read what changed since the last pass and mark the items that read it.
    pub fn observe(&self, store: &Store) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some((index, local)) = state.watermark else {
            let feed = store.changes_since(i64::MAX as u64, i64::MAX)?;
            state.watermark = Some((feed.index, feed.local));
            return Ok(());
        };
        let feed = store.changes_since(index, local)?;
        state.watermark = Some((feed.index, feed.local));
        let State { items, readers, .. } = &mut *state;
        for change in &feed.changes {
            for key in change_keys(change) {
                for item in readers.get(&key).into_iter().flatten() {
                    if let Some(item) = items.get_mut(item) {
                        item.dirty = true;
                    }
                }
            }
        }
        Ok(())
    }

    /// Look again at every exec runtime an item read (`exec:{runtime}`), and mark the items that
    /// read one whose status changed since the last look.
    pub fn observe_execs(&self, observe: impl Fn(&str) -> Option<String>) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let keys = state
            .readers
            .keys()
            .filter(|key| key.starts_with("exec:"))
            .cloned()
            .collect::<Vec<_>>();
        state.execs.retain(|key, _| keys.contains(key));
        for key in keys {
            let status = observe(&key["exec:".len()..]);
            let previous = state.execs.insert(key.clone(), status.clone());
            // Evaluations wait only on running runners, so the first look counts any other status
            // as a change: the runner may have stopped after the evaluation saw it run.
            let changed = match previous {
                Some(previous) => previous != status,
                None => status.as_deref() != Some("running"),
            };
            if changed {
                let readers = state.readers.get(&key).cloned().unwrap_or_default();
                for item in readers {
                    if let Some(item) = state.items.get_mut(&item) {
                        item.dirty = true;
                    }
                }
            }
        }
    }

    /// Whether `item` needs evaluating at `now`: it is new, something it read changed, or its time
    /// has come.
    pub fn needs(&self, item: &str, now: u128) -> bool {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state
            .items
            .get(item)
            .is_none_or(|item| item.dirty || item.due.is_some_and(|due| due <= now))
    }

    /// Remember what `item`'s evaluation read and when its result could next change with time.
    pub fn evaluated(&self, item: &str, reads: BTreeSet<String>, due: Option<u128>) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let State { items, readers, .. } = &mut *state;
        if let Some(previous) = items.get(item) {
            for key in &previous.reads {
                if let Some(set) = readers.get_mut(key) {
                    set.remove(item);
                    if set.is_empty() {
                        readers.remove(key);
                    }
                }
            }
        }
        for key in &reads {
            readers
                .entry(key.clone())
                .or_default()
                .insert(item.to_owned());
        }
        items.insert(
            item.to_owned(),
            Item {
                reads,
                due,
                dirty: false,
            },
        );
    }

    /// Forget items of `kind` (a subject prefix such as `mission-run/`) that are no longer active.
    pub fn retain(&self, prefix: &str, active: &BTreeSet<String>) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let gone = state
            .items
            .keys()
            .filter(|item| item.starts_with(prefix) && !active.contains(*item))
            .cloned()
            .collect::<Vec<_>>();
        for item in gone {
            self.forget_locked(&mut state, &item);
        }
    }

    fn forget_locked(&self, state: &mut State, item: &str) {
        if let Some(previous) = state.items.remove(item) {
            for key in previous.reads {
                if let Some(set) = state.readers.get_mut(&key) {
                    set.remove(item);
                    if set.is_empty() {
                        state.readers.remove(&key);
                    }
                }
            }
        }
    }
}

/// CPU time this thread has used.
pub fn thread_cpu() -> std::time::Duration {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: time points to a valid timespec; the clock reads this thread only.
    if unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut time) } != 0 {
        return std::time::Duration::ZERO;
    }
    std::time::Duration::new(time.tv_sec as u64, time.tv_nsec as u32)
}

/// When a run's result could next change with time alone: the earliest lease expiry, retry
/// backoff, step timeout or run deadline still ahead of `now`.
pub fn run_due(run: &crate::model::MissionRunView, now: u128) -> Option<u128> {
    let mut times = Vec::new();
    times.extend(run.deadline_at_unix_ms);
    for step in &run.steps {
        times.extend(step.claim_expires_at_unix_ms);
        times.extend(step.not_before_unix_ms);
        if let (Some(started), Some(timeout)) = (step.execution_started_at_unix_ms, step.timeout_ms)
        {
            times.push(
                started
                    .saturating_add(u128::from(timeout))
                    .saturating_add(u128::from(step.timeout_extension_ms)),
            );
        }
    }
    times.into_iter().filter(|time| *time > now).min()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(subject: &str, kind: &str, actor: Option<&str>, body: &str) -> Change {
        Change {
            subject: subject.into(),
            kind: kind.into(),
            actor: actor.map(Into::into),
            body: body.into(),
        }
    }

    #[test]
    fn a_change_marks_only_the_items_that_read_it() {
        let incremental = Incremental::default();
        incremental.evaluated(
            "mission-run/a",
            BTreeSet::from(["step-run/a/1".to_owned()]),
            None,
        );
        incremental.evaluated(
            "mission-run/b",
            BTreeSet::from(["mailbox:agent/x".to_owned()]),
            Some(100),
        );
        assert!(!incremental.needs("mission-run/a", 0));
        assert!(!incremental.needs("mission-run/b", 50));
        assert!(incremental.needs("mission-run/b", 100), "its time came");
        assert!(incremental.needs("mission-run/c", 0), "a new item");
        let keys = change_keys(&change(
            "message/m",
            "message.sent",
            Some("person/p"),
            r#"{"fields":{"to":"agent/x"}}"#,
        ));
        assert!(keys.contains(&"mailbox:agent/x".to_owned()));
        assert!(keys.contains(&"actor:person/p".to_owned()));
        assert!(keys.contains(&"kind:message.sent".to_owned()));
    }
}
