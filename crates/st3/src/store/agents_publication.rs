//! One immutable complete current agents view. Transport framing and history are not owned here.

use std::collections::VecDeque;
use std::ops::Range;
use std::sync::{Arc, OnceLock, atomic::{AtomicBool, Ordering}};

use anyhow::{Context as _, Result};
use parking_lot::Mutex;
use serde::{Serialize, Serializer, ser::SerializeStruct as _};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use tokio::sync::watch;
use uuid::Uuid;

// JSON clients represent revisions as safe integers. Exhaustion starts another epoch.
const MAX_REVISION: u64 = (1 << 53) - 1;
// Recent bases a target keeps compared row sets for; subscribers mostly share one or two.
const DIFF_CACHE: usize = 16;

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
    encoding: OnceLock<AgentsEncoding>,
}

impl AgentsPublication {
    pub fn metadata(&self) -> &AgentsPublicationMetadata { &self.metadata }
    pub fn rows(&self) -> &[Value] { self.rows.as_slice() }
    pub fn order(&self) -> &[String] { self.order.as_slice() }

    /// Shared complete encoding, without a transport frame-size or row-count limit.
    /// Initialization happens outside the owner lock, and only once for this publication.
    pub fn encoded(&self) -> &Arc<[u8]> { &self.encoding().body }

    /// The delivered-row fingerprints. Holding it does not retain rows or the encoded body.
    pub fn summary(&self) -> &Arc<AgentsRowsSummary> { &self.encoding().summary }

    /// The canonical encoding of `metadata`, a slice of the complete body.
    pub fn encoded_metadata(&self) -> &[u8] {
        let encoding = self.encoding();
        &encoding.body[encoding.metadata.clone()]
    }

    /// Rows `range`, encoded and comma-separated exactly as in the complete body.
    pub fn encoded_rows(&self, range: Range<usize>) -> &[u8] {
        let encoding = self.encoding();
        encoded_span(&encoding.body, &encoding.rows, range)
    }

    /// The order IDs of rows `range`, encoded and comma-separated exactly as in the complete body.
    pub fn encoded_ids(&self, range: Range<usize>) -> &[u8] {
        let encoding = self.encoding();
        encoded_span(&encoding.body, &encoding.ids, range)
    }

    /// Which rows changed since `base`, when it is an earlier revision of this epoch. A diff
    /// holds row positions and removed IDs only, never the base publication.
    pub fn diff_from(&self, base: &AgentsRowsSummary) -> Option<Arc<AgentsRowsDiff>> {
        if base.epoch != self.metadata.node_epoch || base.revision >= self.metadata.revision {
            return None;
        }
        let encoding = self.encoding();
        if let Some(diff) = encoding.diffs.lock().iter().find(|diff| diff.base_revision == base.revision) {
            return Some(Arc::clone(diff));
        }
        // Compare outside the cache lock; a racing duplicate computes the same diff.
        let target = &encoding.summary;
        let upserts: Box<[u32]> = (0..target.rows.len()).filter(|&index| {
            let row = &target.rows[index];
            base.fingerprint(&row.id) != Some(&row.fingerprint)
        }).map(|index| u32::try_from(index).expect("a roster has fewer than 2^32 rows")).collect();
        let upsert_bytes = upserts.iter().map(|&index| encoding.rows[index as usize].len()).sum::<usize>()
            + upserts.len().saturating_sub(1);
        let mut removes = Vec::new();
        let mut remove_count = 0;
        for row in base.rows.iter().filter(|row| target.position(&row.id).is_none()) {
            if remove_count > 0 { removes.push(b','); }
            serde_json::to_writer(&mut removes, &*row.id).expect("an ID serializes to JSON");
            remove_count += 1;
        }
        let diff = Arc::new(AgentsRowsDiff {
            base_revision: base.revision, upserts, upsert_bytes, removes: removes.into(), remove_count,
        });
        let mut diffs = encoding.diffs.lock();
        if diffs.len() == DIFF_CACHE { diffs.pop_front(); }
        diffs.push_back(Arc::clone(&diff));
        Some(diff)
    }

    fn encoding(&self) -> &AgentsEncoding {
        self.encoding.get_or_init(|| AgentsEncoding::new(self))
    }
}

fn encoded_span<'a>(body: &'a [u8], spans: &[Range<usize>], range: Range<usize>) -> &'a [u8] {
    if range.is_empty() { return &[]; }
    &body[spans[range.start].start..spans[range.end - 1].end]
}

/// One complete body plus the byte ranges framing slices from it. Rows are not encoded twice.
#[derive(Debug)]
struct AgentsEncoding {
    body: Arc<[u8]>,
    metadata: Range<usize>,
    rows: Box<[Range<usize>]>,
    ids: Box<[Range<usize>]>,
    summary: Arc<AgentsRowsSummary>,
    diffs: Mutex<VecDeque<Arc<AgentsRowsDiff>>>,
}

impl AgentsEncoding {
    /// Writes exactly the bytes `Serialize` produces, recording where each part lies.
    fn new(publication: &AgentsPublication) -> Self {
        const DEFECT: &str = "publication values and integer metadata serialize to JSON";
        let mut body = Vec::new();
        body.extend_from_slice(b"{\"publication\":");
        let start = body.len();
        serde_json::to_writer(&mut body, &publication.metadata).expect(DEFECT);
        let metadata = start..body.len();
        body.extend_from_slice(b",\"items\":[");
        let rows = encode_list(&mut body, publication.rows.iter());
        body.extend_from_slice(b"],\"order\":[");
        let ids = encode_list(&mut body, publication.order.iter());
        body.extend_from_slice(b"],\"has_more\":false}");
        let summary_rows: Box<[SummaryRow]> = publication.order.iter().zip(rows.iter()).map(|(id, range)| {
            SummaryRow { id: id.as_str().into(), fingerprint: Sha256::digest(&body[range.clone()]).into() }
        }).collect();
        let mut by_id: Box<[u32]> = (0..summary_rows.len())
            .map(|index| u32::try_from(index).expect("a roster has fewer than 2^32 rows")).collect();
        by_id.sort_unstable_by(|left, right| summary_rows[*left as usize].id.cmp(&summary_rows[*right as usize].id));
        let summary = Arc::new(AgentsRowsSummary {
            epoch: publication.metadata.node_epoch, revision: publication.metadata.revision,
            rows: summary_rows, by_id,
        });
        Self { body: body.into(), metadata, rows, ids, summary, diffs: Mutex::new(VecDeque::new()) }
    }
}

fn encode_list<T: Serialize>(body: &mut Vec<u8>, values: impl Iterator<Item = T>) -> Box<[Range<usize>]> {
    values.enumerate().map(|(index, value)| {
        if index > 0 { body.push(b','); }
        let start = body.len();
        serde_json::to_writer(&mut *body, &value).expect("publication values serialize to JSON");
        start..body.len()
    }).collect()
}

#[derive(Debug)]
struct SummaryRow {
    id: Box<str>,
    fingerprint: [u8; 32],
}

/// What a subscriber was sent: epoch, revision, order and a SHA-256 of each exact encoded row.
/// Computed once per publication and shared; it holds no row values and no body.
#[derive(Debug)]
pub struct AgentsRowsSummary {
    epoch: Uuid,
    revision: u64,
    rows: Box<[SummaryRow]>,
    /// Row positions sorted by ID.
    by_id: Box<[u32]>,
}

impl AgentsRowsSummary {
    pub fn epoch(&self) -> Uuid { self.epoch }
    pub fn revision(&self) -> u64 { self.revision }
    pub fn len(&self) -> usize { self.rows.len() }
    pub fn is_empty(&self) -> bool { self.rows.is_empty() }
    pub fn ids(&self) -> impl Iterator<Item = &str> { self.rows.iter().map(|row| &*row.id) }

    pub fn position(&self, id: &str) -> Option<usize> {
        self.by_id.binary_search_by(|index| (*self.rows[*index as usize].id).cmp(id))
            .ok().map(|found| self.by_id[found] as usize)
    }

    pub fn fingerprint(&self, id: &str) -> Option<&[u8; 32]> {
        self.position(id).map(|index| &self.rows[index].fingerprint)
    }

    /// Retained bytes, for measuring what a delivered cursor costs.
    pub fn heap_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.rows.iter().map(|row| std::mem::size_of::<SummaryRow>() + row.id.len()).sum::<usize>()
            + self.by_id.len() * std::mem::size_of::<u32>()
    }
}

/// Rows a target revision changed since one base revision of its epoch.
#[derive(Debug)]
pub struct AgentsRowsDiff {
    base_revision: u64,
    upserts: Box<[u32]>,
    upsert_bytes: usize,
    /// Removed IDs, encoded and comma-separated.
    removes: Box<[u8]>,
    remove_count: usize,
}

impl AgentsRowsDiff {
    pub fn base_revision(&self) -> u64 { self.base_revision }
    /// Target row positions whose encoded row is new or differs, in target order.
    pub fn upserts(&self) -> impl ExactSizeIterator<Item = usize> + '_ { self.upserts.iter().map(|&index| index as usize) }
    /// Bytes of the upserted rows encoded and comma-separated.
    pub fn upsert_bytes(&self) -> usize { self.upsert_bytes }
    pub fn encoded_removes(&self) -> &[u8] { &self.removes }
    pub fn remove_count(&self) -> usize { self.remove_count }
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

    pub(crate) fn subscribe(&self) -> (watch::Receiver<Option<Arc<AgentsPublication>>>, bool) {
        // Serialize first-demand registration so concurrent sockets cannot each ask for
        // their own refresh, or both miss the transition from no subscribers.
        let _state = self.state.lock();
        let first = self.published.receiver_count() == 0;
        (self.published.subscribe(), first)
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
                rows, order, encoding: OnceLock::new(),
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

    fn publish(owner: &Owner, rows: Vec<Value>) -> Arc<AgentsPublication> {
        let prepared = Prepared::new(AgentsStatusWatermark { store_index: 1, local_frontier: 1 }, 1, None, rows).unwrap();
        owner.finish(owner.begin(), prepared).unwrap().publication
    }

    #[test]
    fn agents_publication_indexed_encoding_is_the_serialized_body() {
        let owner = Owner::default();
        let rows = vec![json!({"id":"agent/a","name":"\"é\u{1}"}), json!({"id":"agent/\"b","name":"B"})];
        let publication = publish(&owner, rows.clone());
        assert_eq!(&publication.encoded()[..], serde_json::to_vec(&*publication).unwrap());
        assert_eq!(publication.encoded_metadata(), serde_json::to_vec(publication.metadata()).unwrap());
        assert_eq!(publication.encoded_rows(0..2), serde_json::to_string(&rows).unwrap().trim_matches(['[', ']']).as_bytes());
        assert_eq!(publication.encoded_rows(1..2), serde_json::to_vec(&rows[1]).unwrap());
        assert_eq!(publication.encoded_ids(0..2), br#""agent/a","agent/\"b""#);
        assert!(publication.encoded_rows(1..1).is_empty());
        let summary = publication.summary();
        assert_eq!(summary.position("agent/\"b"), Some(1));
        let fingerprint: [u8; 32] = Sha256::digest(serde_json::to_vec(&rows[0]).unwrap()).into();
        assert_eq!(summary.fingerprint("agent/a"), Some(&fingerprint));
        assert_eq!(summary.ids().collect::<Vec<_>>(), ["agent/a", "agent/\"b"]);
    }

    #[test]
    fn agents_publication_diff_is_exact_cached_and_bounded() {
        let owner = Owner::default();
        let base = publish(&owner, vec![json!({"id":"a","v":1}), json!({"id":"b","v":1}), json!({"id":"c","v":1})]);
        let target = publish(&owner, vec![json!({"id":"c","v":1}), json!({"id":"a","v":2}), json!({"id":"d","v":1})]);
        let diff = target.diff_from(base.summary()).unwrap();
        assert_eq!(diff.base_revision(), base.metadata().revision);
        assert_eq!(diff.upserts().collect::<Vec<_>>(), [1, 2]);
        assert_eq!(diff.upsert_bytes(), target.encoded_rows(1..3).len());
        assert_eq!(diff.encoded_removes(), br#""b""#);
        assert_eq!(diff.remove_count(), 1);
        assert!(Arc::ptr_eq(&diff, &target.diff_from(base.summary()).unwrap()));
        assert!(target.diff_from(target.summary()).is_none());
        assert!(base.diff_from(target.summary()).is_none());
        let mut bases = Vec::new();
        for marker in 0..=DIFF_CACHE {
            bases.push(publish(&owner, vec![json!({"id":"a","v":marker})]));
        }
        let last = publish(&owner, vec![json!({"id":"a","v":"last"})]);
        for base in &bases { last.diff_from(base.summary()).unwrap(); }
        assert_eq!(last.encoding().diffs.lock().len(), DIFF_CACHE);
        owner.reset();
        let other_epoch = publish(&owner, vec![json!({"id":"a","v":"last"})]);
        assert!(other_epoch.diff_from(last.summary()).is_none());
    }
}
