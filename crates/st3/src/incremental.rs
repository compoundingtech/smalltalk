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
use sha2::Digest as _;

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
    /// Constant-time conservative invalidation when discovery or fanout exceeds its budget.
    generation: u64,
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
    /// Bounded fingerprints of fresh declaration/context inputs, never authority answers.
    contexts: HashMap<String, [u8; 32]>,
    /// When each polled key (such as a terminal's screen) was last looked at.
    polled_at: HashMap<String, u128>,
}

/// How often a pass evaluates every item even when nothing marked them, counting what it corrects.
pub const FULL_PASS_INTERVAL_MS: u128 = 60_000;
const CONTEXT_KEYS: usize = 4096;
const CONTEXT_KEY_BYTES: usize = 4096;
const CONTEXT_BYTES: usize = 1024 * 1024;

struct ContextDigest {
    hash: sha2::Sha256,
    remaining: usize,
}

impl std::io::Write for ContextDigest {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.remaining {
            return Err(std::io::Error::other(
                "reconcile context exceeds byte budget",
            ));
        }
        self.hash.update(bytes);
        self.remaining -= bytes.len();
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct Item {
    reads: BTreeSet<String>,
    due: Option<u128>,
    dirty: bool,
    generation: u64,
}

/// The keys a change can affect: its subject, its actor's work, its kind, and for some kinds a
/// key a read notes for subjects it cannot name in advance.
pub fn change_keys(change: &Change) -> Vec<String> {
    let body = serde_json::from_str::<Value>(&change.body).ok();
    let mut keys = Vec::new();
    visit_change_keys(change, body.as_ref(), |key| {
        keys.push(key.to_owned());
        true
    });
    keys
}

/// Stop BEFORE cloning the next dependency key. The parsed JSON remains covered by the feed's
/// byte budget; in particular, a large members/retired object is never cloned into an unbounded
/// second vector. Incomplete expansion is usable only for conservative global invalidation.
fn bounded_change_keys(change: &Change, budget: usize) -> Result<(Vec<String>, bool)> {
    let body: Value = serde_json::from_str(&change.body)?;
    let mut keys = Vec::new();
    let complete = visit_change_keys(change, Some(&body), |key| {
        if keys.len() == budget {
            return false;
        }
        keys.push(key.to_owned());
        true
    });
    Ok((keys, complete))
}

fn visit_change_keys(
    change: &Change,
    body: Option<&Value>,
    mut visit: impl FnMut(&str) -> bool,
) -> bool {
    let mut agent = false;
    let mut emit = |key: &str| {
        agent |= key.starts_with("agent/");
        visit(key)
    };
    if !emit(&change.subject) || !emit(&format!("kind:{}", change.kind)) {
        return false;
    }
    if let Some(actor) = &change.actor
        && !emit(&format!("actor:{actor}"))
    {
        return false;
    }
    let field = |name: &str| -> Option<&str> {
        let body = body?;
        body.get("fields")
            .unwrap_or(body)
            .get(name)
            .and_then(Value::as_str)
    };
    let fields: &[(&str, &str)] = match change.kind.as_str() {
        "message.sent" => &[("to", "mailbox:")],
        "mission-run.created" => &[
            ("root_mission_run", "children:"),
            ("parent_step_run", "children-of-step:"),
        ],
        "intent.desired" => &[
            ("owner_run", "owned:"),
            ("owner_step", "owned-step:"),
            ("previous_owner_step", "owned-step:"),
            ("previous_owner_generation", ""),
            ("owner_generation", ""),
        ],
        "run-generation.created" => &[("run", "generations:")],
        "subscription.mission-started" => &[("mission_run", "subscription-run:")],
        _ => &[],
    };
    for (name, prefix) in fields {
        if let Some(value) = field(name)
            && !emit(&format!("{prefix}{value}"))
        {
            return false;
        }
    }
    if change.kind == "owned-set.revised" {
        if !emit("kind:intent.desired") {
            return false;
        }
        if let Some(body) = body {
            for map in ["members", "retired"] {
                if let Some(members) = body["fields"]["body"][map].as_object() {
                    for key in members.keys() {
                        if !emit(key) {
                            return false;
                        }
                    }
                }
            }
        }
    }
    if matches!(change.kind.as_str(), "intent.desired" | "owned-set.revised") && agent {
        return visit("desired-kind:agent");
    }
    true
}

impl Incremental {
    /// Read what changed since the last pass and mark the items that read it.
    pub fn observe(&self, store: &Store) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some((index, local)) = state.watermark else {
            let feed = match store.reconcile_changes_since(i64::MAX as u64, i64::MAX) {
                Ok(observed) => observed.feed,
                Err(error) => {
                    Self::invalidate_locked(&mut state);
                    return Err(error);
                }
            };
            state.watermark = Some((feed.index, feed.local));
            return Ok(());
        };
        let observed = match store.reconcile_changes_since(index, local) {
            Ok(observed) => observed,
            Err(error) => {
                Self::invalidate_locked(&mut state);
                return Err(error);
            }
        };
        let feed = observed.feed;
        state.watermark = Some((feed.index, feed.local));
        if observed.invalidate_all {
            Self::invalidate_locked(&mut state);
            return Ok(());
        }
        // Bound discovery expansion, including a single owned-set receipt's large membership
        // and a widely shared dependency. No partial fanout is allowed to stand as clean.
        let mut keys_seen = 0_usize;
        let mut readers_seen = 0_usize;
        let mut affected = BTreeSet::new();
        let mut overflow = false;
        'changes: for change in &feed.changes {
            let (keys, complete) = match bounded_change_keys(change, 4096 - keys_seen) {
                Ok(expanded) => expanded,
                Err(_) => {
                    overflow = true;
                    break;
                }
            };
            if !complete {
                overflow = true;
                break;
            }
            keys_seen += keys.len();
            for key in keys {
                for item in state.readers.get(&key).into_iter().flatten() {
                    readers_seen += 1;
                    if readers_seen > 1024 {
                        overflow = true;
                        break 'changes;
                    }
                    affected.insert(item.clone());
                }
            }
        }
        if overflow {
            Self::invalidate_locked(&mut state);
        } else {
            for item in affected {
                if let Some(item) = state.items.get_mut(&item) {
                    item.dirty = true;
                }
            }
        }
        Ok(())
    }

    fn invalidate_locked(state: &mut State) {
        // A wrapped generation could equal an ancient item's generation. Clearing only at
        // that impossible-in-practice boundary preserves the conservative contract too.
        if let Some(next) = state.generation.checked_add(1) {
            state.generation = next;
        } else {
            state.items.clear();
            state.readers.clear();
            state.generation = 0;
        }
        // Generation mismatch already selects every retained item. Keep the periodic safety
        // clock: after those reads succeed, there is no second forced inventory evaluation.
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

    /// Fingerprint fresh inputs without building a serialized copy. Oversized, unencodable,
    /// or unretained contexts always select their readers; no partial fingerprint is cached.
    /// This catches old/new declaration and auxiliary-input changes, not hidden DB mutations.
    pub(crate) fn observe_context(&self, key: &str, value: &impl serde::Serialize) -> bool {
        let mut digest = ContextDigest {
            hash: sha2::Sha256::new(),
            remaining: CONTEXT_BYTES,
        };
        let encoded = serde_json::to_writer(&mut digest, value).is_ok();
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !encoded
            || key.len() > CONTEXT_KEY_BYTES
            || (!state.contexts.contains_key(key) && state.contexts.len() >= CONTEXT_KEYS)
        {
            Self::mark_locked(&mut state, key);
            return false;
        }
        let digest: [u8; 32] = digest.hash.finalize().into();
        if state.contexts.get(key) != Some(&digest) {
            state.contexts.insert(key.to_owned(), digest);
            Self::mark_locked(&mut state, key);
        }
        true
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
        if state
            .readers
            .get(key)
            .is_some_and(|items| items.len() > 1024)
        {
            Self::invalidate_locked(state);
            return;
        }
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
        state.items.get(item).is_none_or(|item| {
            item.generation != state.generation
                || item.dirty
                || item.due.is_some_and(|due| due <= now)
        })
    }

    /// Remember what `item`'s evaluation read and when its result could next change with time.
    pub fn evaluated(&self, item: &str, reads: BTreeSet<String>, due: Option<u128>) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let generation = state.generation;
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
                generation,
            },
        );
    }

    /// Forget items of `kind` (a subject prefix such as `mission-run/`) that are no longer active.
    pub fn retain(&self, prefix: &str, active: &BTreeSet<String>) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state
            .contexts
            .retain(|key, _| !key.starts_with(prefix) || active.contains(key));
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
    fn fresh_contexts_select_both_old_and_new_dependencies_and_rebirth_is_new() {
        let incremental = Incremental::default();
        for name in ["observer:a", "observer:b"] {
            assert!(incremental.observe_context(name, &"initial"));
            incremental.evaluated(name, BTreeSet::from([name.to_owned()]), None);
        }
        assert!(incremental.observe_context("observer:a", &"initial"));
        assert!(!incremental.needs("observer:a", 0));
        for name in ["observer:a", "observer:b"] {
            assert!(incremental.observe_context(name, &"subscription moved"));
            assert!(
                incremental.needs(name, 0),
                "old and new contexts both change"
            );
        }
        incremental.retain("observer:", &BTreeSet::from(["observer:b".into()]));
        assert!(incremental.observe_context("observer:a", &"initial"));
        assert!(
            incremental.needs("observer:a", 0),
            "removed item cannot reuse a prior evaluation"
        );
        assert!(
            !incremental.reads_of("observer:b").is_empty(),
            "complete membership retains untouched item"
        );
    }

    #[test]
    fn unknown_contexts_are_never_retained_as_clean_and_storage_is_bounded() {
        let incremental = Incremental::default();
        incremental.evaluated("observer:a", BTreeSet::from(["observer:a".into()]), None);
        assert!(!incremental.observe_context("observer:a", &"x".repeat(CONTEXT_BYTES + 1)));
        assert!(incremental.needs("observer:a", 0));
        assert!(incremental.state.lock().unwrap().contexts.is_empty());
        assert!(!incremental.observe_context(&"k".repeat(CONTEXT_KEY_BYTES + 1), &1));
        assert!(incremental.state.lock().unwrap().contexts.is_empty());
        for n in 0..CONTEXT_KEYS {
            assert!(incremental.observe_context(&format!("context:{n}"), &n));
        }
        incremental.evaluated("overflow", BTreeSet::from(["overflow".into()]), None);
        assert!(!incremental.observe_context("overflow", &1));
        assert!(incremental.needs("overflow", 0));
        assert_eq!(
            incremental.state.lock().unwrap().contexts.len(),
            CONTEXT_KEYS
        );
        incremental.retain("context:", &BTreeSet::new());
        assert!(incremental.observe_context("overflow", &1));
    }

    #[test]
    fn shared_dependency_overflow_invalidates_even_unvisited_items_without_a_second_full_pass() {
        let incremental = Incremental::default();
        assert!(incremental.take_full_pass("observer", 0));
        for n in 0..1025 {
            incremental.evaluated(
                &format!("observer:{n}"),
                BTreeSet::from(["shared".into()]),
                None,
            );
        }
        incremental.evaluated("otherwise-unrelated", BTreeSet::new(), None);
        incremental.touch("shared");
        for name in ["observer:0", "observer:1024", "otherwise-unrelated"] {
            assert!(incremental.needs(name, 0));
        }
        assert!(!incremental.take_full_pass("observer", 1));
        incremental.evaluated("otherwise-unrelated", BTreeSet::new(), Some(10));
        assert!(!incremental.needs("otherwise-unrelated", 9));
        assert!(incremental.needs("otherwise-unrelated", 10));
    }

    #[test]
    fn append_discovery_fanout_overflow_selects_items_beyond_the_traversal_budget() {
        let store = Store::open_memory("node").unwrap();
        let incremental = Incremental::default();
        incremental.observe(&store).unwrap();
        for n in 0..1025 {
            incremental.evaluated(
                &format!("reader:{n}"),
                BTreeSet::from(["kind:resource.observed".into()]),
                None,
            );
        }
        incremental.evaluated("otherwise-unrelated", BTreeSet::new(), None);
        observe(&store, "changed");
        incremental.observe(&store).unwrap();
        for name in ["reader:0", "reader:1024", "otherwise-unrelated"] {
            assert!(incremental.needs(name, 0));
        }
    }

    #[test]
    fn one_large_membership_receipt_stops_materialization_and_invalidates_all_readers() {
        let members = (0..4200)
            .map(|n| {
                (
                    format!("agent/{n:04}"),
                    serde_json::json!({
                        "kind":"agent", "claim":"a".repeat(64), "revision":"b".repeat(64)
                    }),
                )
            })
            .collect::<serde_json::Map<String, Value>>();
        let mut receipt = serde_json::json!({"fields":{"revision":"c".repeat(64), "body":{
            "previous":null, "source":{"repository":"acme/repo", "ref":"refs/heads/main",
                "sha":"d".repeat(40), "sequence":1},
            "bundle_digest":"e".repeat(64), "members":members, "retired":{}, "adoptions":{}
        }}});
        let revision: crate::store::owned_sets::Revision =
            serde_json::from_value(receipt["fields"]["body"].clone()).unwrap();
        receipt["fields"]["revision"] =
            Value::String(smallclaims::hash::canonical_hash(&revision).unwrap());
        let large = change(
            "owned-set/large",
            "owned-set.revised",
            None,
            &receipt.to_string(),
        );
        assert!(large.body.len() + large.subject.len() + large.kind.len() < 1024 * 1024);
        let (keys, complete) = bounded_change_keys(&large, 4096).unwrap();
        assert!(!complete);
        assert_eq!(
            keys.len(),
            4096,
            "no cloned membership suffix beyond the budget"
        );
        assert!(!keys.contains(&"agent/4199".to_owned()));
        let (empty, complete) = bounded_change_keys(&large, 0).unwrap();
        assert!(!complete && empty.is_empty());
        // The same producer retains every old key for a small, fully expanded receipt, and
        // the boundary is inclusive rather than silently truncating the final agent-kind key.
        let small = change_keys(&change(
            "owned-set/small",
            "owned-set.revised",
            None,
            r#"{"fields":{"body":{"members":{"agent/a":{}},"retired":{"agent/b":{}}}}}"#,
        ));
        let small_change = change(
            "owned-set/small",
            "owned-set.revised",
            None,
            r#"{"fields":{"body":{"members":{"agent/a":{}},"retired":{"agent/b":{}}}}}"#,
        );
        assert_eq!(
            small,
            [
                "owned-set/small",
                "kind:owned-set.revised",
                "kind:intent.desired",
                "agent/a",
                "agent/b",
                "desired-kind:agent"
            ]
            .map(str::to_owned)
        );
        assert_eq!(
            bounded_change_keys(&small_change, small.len()).unwrap(),
            (small.clone(), true)
        );
        assert!(
            !bounded_change_keys(&small_change, small.len() - 1)
                .unwrap()
                .1
        );

        let store = Store::open_memory("node").unwrap();
        let incremental = Incremental::default();
        incremental.observe(&store).unwrap();
        for (name, dependency) in [
            ("visited", "agent/0000"),
            ("unvisited", "agent/4199"),
            ("unrelated", "doc/elsewhere"),
        ] {
            incremental.evaluated(name, BTreeSet::from([dependency.into()]), None);
        }
        // Reader-only injection of a receipt-shaped hint. This exercises the actual bounded
        // discovery path, not admission, member-reference eligibility or replication proof.
        store.connection.write().execute(
            "INSERT INTO local_observations(after_store_index,subject,kind,body,observed_at_unix_ms)
                VALUES(0,?1,?2,?3,1)",
            rusqlite::params![large.subject, large.kind, large.body],
        ).unwrap();
        let observed = store.reconcile_changes_since(0, 0).unwrap();
        assert!(
            !observed.invalidate_all && observed.feed.changes.len() == 1,
            "the key cap, not the row/byte cap, must select the fallback"
        );
        incremental.observe(&store).unwrap();
        for name in ["visited", "unvisited", "unrelated"] {
            assert!(incremental.needs(name, 0));
        }
    }

    #[test]
    fn appended_local_and_replicated_changes_use_the_same_conservative_selection() {
        let store = Store::open_memory("node").unwrap();
        let foreign = Store::open_memory("foreign").unwrap();
        let incremental = Incremental::default();
        incremental.observe(&store).unwrap();
        for subject in ["resource/foreign", "resource/local", "resource/unrelated"] {
            incremental.evaluated(subject, BTreeSet::from([subject.into()]), None);
        }
        observe(&foreign, "foreign");
        store
            .import_replication("foreign", &foreign.export_replication(0).unwrap())
            .unwrap();
        observe(&store, "local");
        incremental.observe(&store).unwrap();
        assert!(incremental.needs("resource/foreign", 0));
        assert!(incremental.needs("resource/local", 0));
        assert!(!incremental.needs("resource/unrelated", 0));
    }

    #[test]
    fn byte_overflow_and_read_error_invalidate_all_without_advancing_an_error_cursor() {
        let store = Store::open_memory("node").unwrap();
        let incremental = Incremental::default();
        incremental.observe(&store).unwrap();
        incremental.evaluated("unrelated", BTreeSet::new(), None);
        // Admitted, individually small observations cross only the combined byte cap.
        for n in 0..130 {
            store
                .append_claim(&crate::model::ClaimInput {
                    subject: format!("resource/large-{n}"),
                    kind: "resource.observed".into(),
                    actor: None,
                    fields: std::collections::BTreeMap::from([
                        ("kind".into(), Value::String("custom.st3.test".into())),
                        (
                            "facts".into(),
                            serde_json::json!({"large":"x".repeat(8192)}),
                        ),
                    ]),
                    evidence: vec![],
                    expected_subject: None,
                    idempotency_key: None,
                })
                .unwrap();
        }
        incremental.observe(&store).unwrap();
        assert!(incremental.needs("unrelated", 0));
        incremental.evaluated("unrelated", BTreeSet::new(), None);
        let before = incremental.state.lock().unwrap().watermark;
        store
            .connection
            .write()
            .execute(
                "ALTER TABLE local_observations RENAME TO unavailable_local",
                [],
            )
            .unwrap();
        assert!(incremental.observe(&store).is_err());
        assert_eq!(incremental.state.lock().unwrap().watermark, before);
        assert!(incremental.needs("unrelated", 0));
    }
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
                fields: std::collections::BTreeMap::from([
                    ("mission_run".into(), Value::String("mission-run/a".into())),
                    ("request".into(), Value::String("a".repeat(64))),
                ]),
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
