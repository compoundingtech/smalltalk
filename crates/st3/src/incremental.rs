//! Which items a reconcile pass must evaluate.
//!
//! Each item (a mission run, a stop, later live members and intake declarations) remembers what
//! its last evaluation read, and when its result could next change with time alone. A pass reads
//! what changed since the last one (claims, local observations, the PTY snapshot, watched files,
//! exec runners), turns each change into the keys a read would have noted, and marks the items that
//! read any of them. An item needs evaluating when it is new, marked, or due; the others are
//! skipped. Every [`FULL_PASS_INTERVAL_MS`] a section evaluates every item anyway, and an
//! evaluation that writes although its item was not marked is counted as a correction: a write
//! the skipping passes missed. See `doc/fleet/smalltalk/idle-cpu-incremental-design`.

use std::collections::{BTreeSet, HashMap};
use std::sync::Mutex;

use anyhow::Result;
use serde_json::Value;

use crate::store::{Change, Store};

/// Shared, so a file watcher or timer can mark what it changed.
#[derive(Clone, Default)]
pub struct Incremental {
    state: std::sync::Arc<Mutex<State>>,
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
    /// When each section's last full pass ran.
    last_full: HashMap<&'static str, u128>,
    /// The last PTY snapshot, by runtime: `None` before the first, or while it was unavailable.
    ptys: Option<HashMap<String, String>>,
    /// The last value seen for each key observed by value, such as `live-workspaces`.
    values: HashMap<String, String>,
    /// When each polled key (such as a terminal's screen) was last looked at.
    polled_at: HashMap<String, u128>,
}

/// How often a pass evaluates every item even when nothing marked them, counting what it corrects.
pub const FULL_PASS_INTERVAL_MS: u128 = 60_000;

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
        "owned-set.revised" => {
            keys.push("kind:intent.desired".into());
            if let Ok(body) = serde_json::from_str::<Value>(&change.body) {
                for map in ["members", "retired"] {
                    if let Some(members) = body["fields"]["body"][map].as_object() {
                        keys.extend(members.keys().cloned());
                    }
                }
            }
        }
        "intent.desired" => {
            keys.extend(field("owner_run").map(|run| format!("owned:{run}")));
            keys.extend(field("owner_step").map(|step| format!("owned-step:{step}")));
        }
        "run-generation.created" => {
            keys.extend(field("run").map(|run| format!("generations:{run}")));
        }
        "subscription.mission-started" => {
            keys.extend(field("mission_run").map(|run| format!("subscription-run:{run}")));
        }
        _ => {}
    }
    if matches!(change.kind.as_str(), "intent.desired" | "owned-set.revised")
        && keys.iter().any(|key| key.starts_with("agent/"))
    {
        keys.push("desired-kind:agent".into());
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

    /// Mark the items that read `key`, for a change that is not a claim, such as a watched file.
    pub fn touch(&self, key: &str) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Self::mark_locked(&mut state, key);
    }

    /// Whether this pass must evaluate every item of `section`: the first since a start, or the
    /// first after [`FULL_PASS_INTERVAL_MS`]. Records it as the section's last full pass.
    pub fn take_full_pass(&self, section: &'static str, now: u128) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let due = state
            .last_full
            .get(section)
            .is_none_or(|last| now.saturating_sub(*last) >= FULL_PASS_INTERVAL_MS);
        if due {
            state.last_full.insert(section, now);
        }
        due
    }

    /// Compare this pass's PTY snapshot (each runtime's state, as text) with the last one, and mark
    /// the items that read a runtime whose state changed (`pty:{runtime}`). When the snapshot comes
    /// back after it was unavailable, every item that read any runtime is marked.
    pub fn observe_ptys(&self, snapshot: Option<HashMap<String, String>>) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(snapshot) = snapshot else {
            state.ptys = None;
            return;
        };
        let changed = match &state.ptys {
            Some(previous) => previous
                .iter()
                .filter(|(id, value)| snapshot.get(*id) != Some(value))
                .map(|(id, _)| format!("pty:{id}"))
                .chain(
                    snapshot
                        .keys()
                        .filter(|id| !previous.contains_key(*id))
                        .map(|id| format!("pty:{id}")),
                )
                .collect::<Vec<_>>(),
            None => state
                .readers
                .keys()
                .filter(|key| key.starts_with("pty:"))
                .cloned()
                .collect(),
        };
        state.ptys = Some(snapshot);
        for key in changed {
            Self::mark_locked(&mut state, &key);
        }
    }

    /// Mark the items that read `key` when `value` differs from the value last observed for it.
    pub fn observe_value(&self, key: &str, value: String) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.values.get(key) != Some(&value) {
            state.values.insert(key.to_owned(), value);
            Self::mark_locked(&mut state, key);
        }
    }

    /// Remember what an evaluation saw for a polled key, so a later poll marks its readers only
    /// when the value changes from that.
    pub fn saw_value(&self, key: &str, value: String, now: u128) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.values.insert(key.to_owned(), value);
        state.polled_at.insert(key.to_owned(), now);
    }

    /// Look again, at most every `every_ms`, at each key with `prefix` that an item read, and
    /// mark its readers when the value changed. `poll` gets the key without the prefix.
    pub fn poll_values(
        &self,
        prefix: &str,
        every_ms: u128,
        now: u128,
        poll: impl Fn(&str) -> Option<String>,
    ) {
        let keys = {
            let state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state
                .readers
                .keys()
                .filter(|key| key.starts_with(prefix))
                .filter(|key| {
                    state
                        .polled_at
                        .get(*key)
                        .is_none_or(|at| now.saturating_sub(*at) >= every_ms)
                })
                .cloned()
                .collect::<Vec<_>>()
        };
        for key in keys {
            let value = poll(&key[prefix.len()..]).unwrap_or_default();
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.polled_at.insert(key.clone(), now);
            if state.values.get(&key) != Some(&value) {
                state.values.insert(key.clone(), value);
                Self::mark_locked(&mut state, &key);
            }
        }
    }

    fn mark_locked(state: &mut State, key: &str) {
        let State { items, readers, .. } = state;
        for item in readers.get(key).into_iter().flatten() {
            if let Some(item) = items.get_mut(item) {
                item.dirty = true;
            }
        }
    }

    /// The earliest time an item whose name starts with `prefix` comes due.
    pub fn next_due(&self, prefix: &str) -> Option<u128> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state
            .items
            .iter()
            .filter(|(name, _)| name.starts_with(prefix))
            .filter_map(|(_, item)| item.due)
            .min()
    }

    /// When the next section's full pass is due: the earliest last full pass plus
    /// [`FULL_PASS_INTERVAL_MS`]. A due time an item failed to record still comes round then.
    pub fn next_full_pass(&self) -> Option<u128> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state
            .last_full
            .values()
            .min()
            .map(|last| last.saturating_add(FULL_PASS_INTERVAL_MS))
    }

    /// What `item`'s last evaluation read.
    pub fn reads_of(&self, item: &str) -> BTreeSet<String> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state
            .items
            .get(item)
            .map(|item| item.reads.clone())
            .unwrap_or_default()
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
    run.deadline_at_unix_ms
        .filter(|time| *time > now)
        .into_iter()
        .chain(run.steps.iter().filter_map(|step| step_due(step, now)))
        .min()
}

/// When a step's view could next change with time alone: its lease expiry, retry backoff or
/// timeout, whichever comes first after `now`.
pub fn step_due(step: &crate::model::StepRunView, now: u128) -> Option<u128> {
    let timeout =
        step.execution_started_at_unix_ms
            .zip(step.timeout_ms)
            .map(|(started, timeout)| {
                started
                    .saturating_add(u128::from(timeout))
                    .saturating_add(u128::from(step.timeout_extension_ms))
            });
    [
        step.claim_expires_at_unix_ms,
        step.not_before_unix_ms,
        timeout,
    ]
    .into_iter()
    .flatten()
    .filter(|time| *time > now)
    .min()
}

#[cfg(test)]
mod tests {
    #[test]
    fn agent_collection_keys_ignore_message_declarations_and_cover_owned_set_members() {
        assert!(
            !super::change_keys(&change("message/m", "intent.desired", None, "{}"))
                .contains(&"desired-kind:agent".to_owned())
        );
        assert!(
            !super::change_keys(&change("agent/a", "harness.observed", None, "{}"))
                .contains(&"desired-kind:agent".to_owned())
        );
        assert!(
            super::change_keys(&change("agent/a", "intent.desired", None, "{}"))
                .contains(&"desired-kind:agent".to_owned())
        );
        assert!(
            super::change_keys(&change(
                "owned-set/x",
                "owned-set.revised",
                None,
                r#"{"fields":{"body":{"members":{"agent/a":{}}}}}"#
            ))
            .contains(&"desired-kind:agent".to_owned())
        );
    }

    use super::*;

    fn change(subject: &str, kind: &str, actor: Option<&str>, body: &str) -> Change {
        Change {
            subject: subject.into(),
            kind: kind.into(),
            actor: actor.map(Into::into),
            body: body.into(),
        }
    }

    fn observe(store: &Store, resource: &str) {
        store
            .append_claim(&crate::model::ClaimInput {
                subject: format!("resource/{resource}"),
                kind: "resource.observed".into(),
                actor: None,
                fields: std::collections::BTreeMap::from([(
                    "kind".into(),
                    Value::String("custom.st3.test".into()),
                )]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some(resource.into()),
            })
            .unwrap();
    }

    /// A correction names what the item wrote: a claim another thread wrote meanwhile, which a
    /// change feed read would include, is not its write.
    #[test]
    fn a_recording_lists_only_its_own_writes() {
        let store = std::sync::Arc::new(Store::open_memory("node").unwrap());
        let other = store.clone();
        let ((), wrote) = smallclaims::touched::record_wrote(|| {
            observe(&store, "own");
            std::thread::spawn(move || observe(&other, "other"))
                .join()
                .unwrap();
        });
        assert_eq!(wrote, ["resource.observed resource/own"]);
        assert!(
            smallclaims::touched::wrote_since(0).is_empty(),
            "nothing records outside it"
        );
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
            Some("person/example"),
            r#"{"fields":{"to":"agent/x"}}"#,
        ));
        assert!(keys.contains(&"mailbox:agent/x".to_owned()));
        assert!(keys.contains(&"actor:person/example".to_owned()));
        assert!(keys.contains(&"kind:message.sent".to_owned()));
    }

    #[test]
    fn a_subscription_start_wakes_only_its_run() {
        let store = Store::open_memory("orchid").unwrap();
        let incremental = Incremental::default();
        incremental.observe(&store).unwrap();
        for run in ["a", "b"] {
            incremental.evaluated(
                &format!("mission-run/{run}"),
                BTreeSet::from([format!("subscription-run:mission-run/{run}")]),
                None,
            );
        }
        store
            .append_claim(&crate::model::ClaimInput {
                subject: "subscription/reviews".into(),
                kind: "subscription.mission-started".into(),
                actor: None,
                fields: std::collections::BTreeMap::from([(
                    "mission_run".into(),
                    Value::String("mission-run/a".into()),
                )]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        incremental.observe(&store).unwrap();
        assert!(incremental.needs("mission-run/a", 0));
        assert!(!incremental.needs("mission-run/b", 0));
    }
}
