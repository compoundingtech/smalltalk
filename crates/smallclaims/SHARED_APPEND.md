# Experimental shared replica appends

Shared replica grouping is disabled by default. A caller must explicitly enable
`WriterConnection::set_shared_appends(true)` and use `batched_append` with its
runtime's reviewed retention policy. Generic writes continue to use `batched`.
This is a draft implementation awaiting correctness, compatibility, retention
and performance verification; it is not a recommendation to enable grouping.

The existing FIFO writer queue already shares SQLite commits. Replica grouping
additionally shares a new batch header among eligible queued claims. There is no
new queue, retry or delay to fill a batch. Each job keeps its existing savepoint
and receives its result after the real transaction commits. The first job of a
transaction uses the serial path. A generic first job keeps that transaction on
the serial path. Writer loans end reuse.

Only new batches created within the current transaction can be extended. Old
claims, headers, payloads and signatures are never rewritten or re-signed.
Grouped claims have different batch IDs, claim IDs and hashes from serial
appends. Idempotency lookup still happens before append and returns the original
accepted claim for the same key and request. Forced-batch calls preserve their
existing behavior and end reuse.

Every group has the same writer, acceptance millisecond, actor and retention
class. Wall time is sampled for every claim. A millisecond change starts another
header, preserving canonical time ordering across origins. Groups contain at
most 32 claims and use a provisional 256 KiB input budget. The heuristic does
not yet prove an encoded CBOR cap when signing-key delegation metadata is large;
that admission proof remains a blocker for enabling grouping. Final sealing now checks the complete serialized CBOR payload, including signatures, against the cap. Blob references
and oversized candidates take the serial path. Duplicate hashes start another
batch and remain two accepted claims. Membership, authority and rule writes
cannot join retained message groups.

Smalltalk's only groupable kinds are the exact allowlist beside its checkpoint
rule selector: message sent, staged, delivered, read and closed, plus work
progress. No prefix admits a kind. Eligibility also checks the current selector;
a regression test fails if an allowlisted kind gains a drop rule. Observations
remain separate. Whole signed envelopes are retained while any claim in them is
retained. If a future rules version makes these kinds droppable, old grouped
envelopes can still retain their neighbours. That rule change needs a review of
grouping policy and existing envelopes.

The provisional frontier is reusable by another job only after savepoint
release. Partial failures, panic rollback, generic writes, changed clock or
writer metadata, schema changes, forced batches and envelope finalization clear
reuse. Row notifications cover frontier mutations; comparison with SQLite's
total change count discards state when truncate deletion or other unhooked
mutations occur. Within the shared scope, temporary row triggers witness
`WITHOUT ROWID` changes through a private rowid table. The dispatcher counts the
original change and witness update together, and applies the same authority
invalidation to the original table. Direct writes to bookkeeping disable reuse.
The scope removes its temporary triggers and table before COMMIT; cleanup errors
roll back the transaction. Unknown changes still discard the frontier, and
setup is attempted only once per transaction. These SQL operations and their
statement-cache cost must be included in enabled/disabled measurements.
A debug SQL oracle checks cached sequence, previous hash, clock,
header, claim membership and finalization before reuse. This path uses ordinary
rusqlite hooks, without preupdate, bindgen or extra SQLite build flags.

Each new experimental batch has an atomic local-only pending marker and a bounded
summary in `meta`. These are excluded from the envelope payload. The actual CBOR cap
check resolves a marker only in the sealing COMMIT. On oversize, the whole sealing
chunk rolls back before the same writer loan publishes bounded, incarnation-checked
fault evidence in a separate transaction. An evidence commit failure remains visible
in process memory; restart still sees pending evidence and cannot report verified.
The read accessor performs one bounded point lookup, never sealing or scanning.
Known faults prevent ordinary sealing from re-encoding the same bad batch, even if
its rows are subsequently deleted. Explicit repair/clear, already-sealed-marker
recovery and an admission bound for grandfathered signing metadata remain unfinished;
no operator recovery procedure or healthy-runtime activation is claimed. Marker,
summary and fault operations also require before/after cost measurements.

Signing keeps its existing point: committed batches are sealed on a later FIFO
writer loan. The payload therefore contains exactly the claims whose savepoints
survived. No unfinished shared state crosses COMMIT or a loan. A previous build
must verify and apply an actual grouped envelope before compatibility is
accepted. Canonical comparisons use an explicit mapping of logical claims,
because the new IDs differ. The canonical tuple and legacy position COUNT are
unchanged.

The append-cost example measures enabled and disabled controls using invented
data at 1, 10 and 100 concurrent calls. It reports achieved claims per batch,
acknowledgement p99, SQL work, shared commits and a separate sealing phase with
signature counts. Source identity, copied-store measurements, crash/restart
negatives, previous-build ingestion, retained bytes and exact-head reviews remain
acceptance gates. Existing claim indexes are retained until every reader and
query plan proves an index redundant.

Enabling also requires that every fleet member runs a build that passed the
old-node gate. Source-built component tests do not qualify deployed daemon
artifacts, native platforms, Smalltalk projections or checkpoint exchange.
