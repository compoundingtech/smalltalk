//! One immutable complete current agents view. Transport framing and history are not owned here.

use std::sync::{Arc, OnceLock, atomic::{AtomicBool, Ordering}};

use anyhow::{Context as _, Result};
use parking_lot::Mutex;
use serde::{Serialize, Serializer, ser::SerializeStruct as _};
use serde_json::Value;
use tokio::sync::watch;
use uuid::Uuid;

// JSON clients represent revisions as safe integers. Exhaustion starts another epoch.
const MAX_REVISION: u64 = (1 << 53) - 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct AgentsStatusWatermark {
    pub store_index: u64,
    pub local_frontier: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct AgentsPublicationMetadata {
    #[serde(serialize_with = "serialize_epoch")]
    pub node_epoch: Uuid,
    pub revision: u64,
    pub status_watermark: AgentsStatusWatermark,
    pub materialized_at_ms: u64,
}

fn serialize_epoch<S: Serializer>(epoch: &Uuid, serializer: S) -> std::result::Result<S::Ok, S::Error> {
    serializer.collect_str(epoch)
}

/// Rows, order and metadata are one object. No caller can mutate a retained revision.
#[derive(Debug)]
pub struct AgentsPublication {
    metadata: AgentsPublicationMetadata,
    rows: Arc<Vec<Value>>,
    order: Arc<Vec<String>>,
    encoded: OnceLock<Arc<[u8]>>,
}

impl AgentsPublication {
    pub fn metadata(&self) -> &AgentsPublicationMetadata { &self.metadata }
    pub fn rows(&self) -> &[Value] { self.rows.as_slice() }
    pub fn order(&self) -> &[String] { self.order.as_slice() }

    /// Shared complete encoding, without a transport frame-size or row-count limit.
    /// Initialization happens outside the owner lock, and only once for this publication.
    pub fn encoded(&self) -> &Arc<[u8]> {
        self.encoded.get_or_init(|| {
            serde_json::to_vec(self)
                .expect("publication values and integer metadata serialize to JSON")
                .into()
        })
    }
}

impl Serialize for AgentsPublication {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let mut object = serializer.serialize_struct("AgentsPublication", 4)?;
        object.serialize_field("publication", &self.metadata)?;
        object.serialize_field("items", self.rows.as_ref())?;
        object.serialize_field("order", self.order.as_ref())?;
        object.serialize_field("has_more", &false)?;
        object.end()
    }
}

pub(crate) struct Prepared {
    pub(crate) watermark: AgentsStatusWatermark,
    pub(crate) materialized_at_ms: u64,
    pub(crate) valid_until_ms: Option<u64>,
    rows: Vec<Value>,
    order: Vec<String>,
}

impl Prepared {
    pub(crate) fn new(
        watermark: AgentsStatusWatermark,
        materialized_at_ms: u64,
        valid_until_ms: Option<u64>,
        rows: Vec<Value>,
    ) -> Result<Self> {
        let order = rows.iter().map(|row| {
            row["id"].as_str().context("a complete agent card has no ID").map(str::to_owned)
        }).collect::<Result<Vec<_>>>()?;
        Ok(Self { watermark, materialized_at_ms, valid_until_ms, rows, order })
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Build {
    epoch: Uuid,
    generation: u64,
    ticket: u64,
}

struct State {
    epoch: Uuid,
    revision: u64,
    generation: u64,
    completed_generation: u64,
    ticket: u64,
    completed_ticket: u64,
    valid_until_ms: Option<u64>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            epoch: Uuid::new_v4(), revision: 0, generation: 1, completed_generation: 0,
            ticket: 0, completed_ticket: 0, valid_until_ms: None,
        }
    }
}

/// The watch value is the sole retained current publication. The state stores no rows.
pub(crate) struct Owner {
    state: Mutex<State>,
    published: watch::Sender<Option<Arc<AgentsPublication>>>,
    source_dirty: AtomicBool,
}

impl Default for Owner {
    fn default() -> Self {
        Self {
            state: Mutex::new(State::default()), published: watch::channel(None).0,
            source_dirty: AtomicBool::new(true),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Discarded {
    Reset,
    Superseded,
}

pub(crate) struct Completed {
    pub(crate) publication: Arc<AgentsPublication>,
    pub(crate) changed: bool,
    pub(crate) dirty: bool,
}

impl Owner {
    pub(crate) fn current(&self) -> Option<Arc<AgentsPublication>> {
        self.published.borrow().as_ref().map(Arc::clone)
    }

    pub(crate) fn subscribe(&self) -> watch::Receiver<Option<Arc<AgentsPublication>>> {
        self.published.subscribe()
    }

    pub(crate) fn has_subscribers(&self) -> bool { self.published.receiver_count() != 0 }

    /// The writer's commit observer must never wait for the publication lock.
    pub(crate) fn source_changed(&self) {
        self.source_dirty.store(true, Ordering::Release);
    }

    /// Register work without consuming an input that arrives during an older build.
    pub(crate) fn dirty(&self) {
        let mut state = self.state.lock();
        let retired = if state.generation == u64::MAX {
            *state = State::default();
            self.published.send_replace(None)
        } else {
            state.generation += 1;
            None
        };
        drop(state);
        drop(retired);
    }

    pub(crate) fn begin(&self) -> Build {
        let mut state = self.state.lock();
        let retired = if state.ticket == u64::MAX {
            *state = State::default();
            self.published.send_replace(None)
        } else { None };
        self.source_dirty.swap(false, Ordering::AcqRel);
        state.ticket += 1;
        let build = Build { epoch: state.epoch, generation: state.generation, ticket: state.ticket };
        drop(state);
        drop(retired);
        build
    }

    pub(crate) fn reset(&self) {
        let mut state = self.state.lock();
        *state = State::default();
        self.source_dirty.store(true, Ordering::Release);
        let retired = self.published.send_replace(None);
        drop(state);
        drop(retired);
    }

    pub(crate) fn needed(&self, now_ms: u64) -> bool {
        let state = self.state.lock();
        state.revision == 0 || state.generation != state.completed_generation
            || state.valid_until_ms.is_some_and(|deadline| now_ms >= deadline)
            || self.source_dirty.load(Ordering::Acquire)
    }

    pub(crate) fn deadline(&self) -> Option<u64> {
        self.state.lock().valid_until_ms
    }

    /// Compare prepared rows outside the owner lock, then bind revision and replacement inside it.
    /// A reset fences old work; newer completion fences an older build's final quiet result.
    pub(crate) fn finish(&self, build: Build, prepared: Prepared) -> std::result::Result<Completed, Discarded> {
        loop {
            let previous = self.current();
            let same_rows = previous.as_ref().is_some_and(|publication| {
                publication.rows.as_ref() == &prepared.rows && publication.order.as_ref() == &prepared.order
            });
            let unchanged = same_rows && previous.as_ref().is_some_and(|publication| {
                publication.metadata.status_watermark == prepared.watermark
            });
            let mut state = self.state.lock();
            if state.epoch != build.epoch { return Err(Discarded::Reset); }
            if state.completed_ticket > build.ticket { return Err(Discarded::Superseded); }
            let current = self.current();
            if !same_publication(previous.as_ref(), current.as_ref()) {
                drop(state);
                continue;
            }
            state.completed_generation = state.completed_generation.max(build.generation);
            state.completed_ticket = build.ticket;
            state.valid_until_ms = prepared.valid_until_ms;
            let dirty = state.generation != state.completed_generation
                || self.source_dirty.load(Ordering::Acquire);
            if unchanged {
                let publication = previous.expect("an unchanged publication exists");
                drop(state);
                return Ok(Completed { publication, changed: false, dirty });
            }
            if state.revision == MAX_REVISION {
                state.epoch = Uuid::new_v4();
                state.revision = 0;
            }
            state.revision += 1;
            let (rows, order) = if same_rows {
                let previous = previous.as_ref().expect("identical rows have a previous publication");
                (Arc::clone(&previous.rows), Arc::clone(&previous.order))
            } else {
                (Arc::new(prepared.rows), Arc::new(prepared.order))
            };
            let publication = Arc::new(AgentsPublication {
                metadata: AgentsPublicationMetadata {
                    node_epoch: state.epoch, revision: state.revision,
                    status_watermark: prepared.watermark, materialized_at_ms: prepared.materialized_at_ms,
                },
                rows, order, encoded: OnceLock::new(),
            });
            let retired = self.published.send_replace(Some(Arc::clone(&publication)));
            drop(state);
            // Releasing a large old roster is also outside the owner lock.
            drop(retired);
            return Ok(Completed { publication, changed: true, dirty });
        }
    }
}

fn same_publication(left: Option<&Arc<AgentsPublication>>, right: Option<&Arc<AgentsPublication>>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => Arc::ptr_eq(left, right),
        (None, None) => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::{Barrier, atomic::{AtomicBool, Ordering}};

    fn prepared(marker: u64, local: u64) -> Prepared {
        Prepared::new(AgentsStatusWatermark { store_index: marker, local_frontier: local }, marker,
            None, vec![json!({"id":"agent/invented", "marker":marker})]).unwrap()
    }

    #[test]
    fn agents_publication_atomic_rows_metadata_and_encoding() {
        let owner = Arc::new(Owner::default());
        let start = Arc::new(Barrier::new(2));
        let done = Arc::new(AtomicBool::new(false));
        let reader = {
            let owner = Arc::clone(&owner);
            let start = Arc::clone(&start);
            let done = Arc::clone(&done);
            std::thread::spawn(move || {
                start.wait();
                let mut seen = 0;
                loop {
                    if let Some(publication) = owner.current() {
                        let revision = publication.metadata().revision;
                        assert!(revision >= seen);
                        assert_eq!(publication.rows()[0]["marker"], revision);
                        let encoded: Value = serde_json::from_slice(publication.encoded()).unwrap();
                        assert_eq!(encoded["publication"]["revision"], encoded["items"][0]["marker"]);
                        seen = revision;
                    }
                    if done.load(Ordering::Acquire) { break; }
                    std::thread::yield_now();
                }
                assert!(seen > 0, "the concurrent observer must inspect a publication");
            })
        };
        start.wait();
        for marker in 1..=500 {
            let result = owner.finish(owner.begin(), prepared(marker, marker)).unwrap();
            assert_eq!(result.publication.metadata().revision, marker);
            std::thread::yield_now();
        }
        done.store(true, Ordering::Release);
        reader.join().unwrap();
    }

    #[test]
    fn agents_publication_monotonic_same_cut_and_retreating_local_frontier() {
        let owner = Owner::default();
        let first = owner.finish(owner.begin(), prepared(1, 8)).unwrap().publication;
        let mut next = prepared(1, 3);
        next.rows[0]["marker"] = json!(2);
        let second = owner.finish(owner.begin(), next).unwrap().publication;
        assert_eq!(first.metadata().node_epoch, second.metadata().node_epoch);
        assert_eq!(second.metadata().revision, first.metadata().revision + 1);
        assert_eq!(second.metadata().status_watermark.local_frontier, 3);
        assert_eq!(first.rows()[0]["marker"], 1);
    }

    #[test]
    fn agents_publication_reset_and_revision_exhaustion_change_epoch() {
        let owner = Owner::default();
        let first = owner.finish(owner.begin(), prepared(1, 1)).unwrap().publication;
        let old_build = owner.begin();
        owner.reset();
        assert!(owner.current().is_none());
        assert_eq!(owner.finish(old_build, prepared(2, 2)).err(), Some(Discarded::Reset));
        let reset = owner.finish(owner.begin(), prepared(2, 2)).unwrap().publication;
        assert_ne!(reset.metadata().node_epoch, first.metadata().node_epoch);
        assert_eq!(reset.metadata().revision, 1);
        owner.state.lock().revision = MAX_REVISION;
        let exhausted = owner.finish(owner.begin(), prepared(3, 3)).unwrap().publication;
        assert_ne!(exhausted.metadata().node_epoch, reset.metadata().node_epoch);
        assert_eq!(exhausted.metadata().revision, 1);
    }

    #[test]
    fn agents_publication_preserves_dirty_work_and_discards_late_old_completion() {
        let owner = Owner::default();
        let old_build = owner.begin();
        owner.dirty();
        let first = owner.finish(old_build, prepared(1, 1)).unwrap();
        assert!(first.dirty);
        assert!(owner.needed(1));
        let late = owner.begin();
        owner.dirty();
        let newer = owner.begin();
        owner.finish(newer, prepared(3, 3)).unwrap();
        assert_eq!(owner.finish(late, prepared(2, 2)).err(), Some(Discarded::Superseded));
        assert_eq!(owner.current().unwrap().rows()[0]["marker"], 3);
        assert!(!owner.needed(3));
    }

    #[test]
    fn agents_publication_reuses_identical_view_without_fabricating_new_time() {
        let owner = Owner::default();
        let first = owner.finish(owner.begin(), prepared(1, 1)).unwrap().publication;
        owner.dirty();
        let mut same = prepared(1, 1);
        same.materialized_at_ms = 100;
        let completed = owner.finish(owner.begin(), same).unwrap();
        assert!(!completed.changed);
        assert!(!completed.dirty);
        assert!(Arc::ptr_eq(&first, &completed.publication));
        assert_eq!(completed.publication.metadata().materialized_at_ms, 1);
    }

    #[test]
    fn agents_publication_preserves_the_final_quiet_source_notice() {
        let owner = Owner::default();
        let first = owner.begin();
        owner.source_changed();
        assert!(owner.finish(first, prepared(1, 1)).unwrap().dirty);
        assert!(owner.needed(1));
        let next = owner.begin();
        assert!(!owner.finish(next, prepared(2, 2)).unwrap().dirty);
        assert!(!owner.needed(2));
    }
}
