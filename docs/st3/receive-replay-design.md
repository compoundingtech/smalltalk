# Receive replay staging design

Draft design for [#2861](https://github.com/compoundingtech/smalltalk/issues/2861).
Rebuild a private projection, copy it into an unpublished generation in bounded pages,
and publish that generation with a small transaction in the same database as the claim log.
Receive acknowledges durable admission and reports projection pending; it never performs a
full rebuild while retaining the live writer in the proposed activated path. Current behavior
is unchanged until an approved conversion and capability gate. Implementation requires storage,
reader routing and client contract approval.

## Source and reported incident

The issue reports a receive writer hold of 226.0 seconds and request wall time of 232.4 seconds.
Its daemon build and triggering fallback guard were not captured. Those observations establish
the held writer and blocked mutations, not that a particular current guard caused that request.
The proposed controls must establish that connection on disposable stores.

The source baseline for this design is `24d78d71b9b99a2614de215c1f1660e1b2fa7d2e`.
The issue's earlier source pin is `3ea6e22a89eb4856d0aa30cac6f5fff997f58f2b`.
At the design baseline:

- `crates/st3/src/api.rs::replication_receive` admits the exchange, validates backlog and applies
  repairs, then requests projection for new data or previously deferred projection. An empty
  receive after catch-up can therefore request projection too.
- `crates/smallclaims/src/store.rs::project_replication_backlog_chunks` retains a foreground
  writer and IMMEDIATE transaction through incremental projection, fallback replay,
  `after_projection`, health/frontier publication and commit. Its 128-claim incremental page
  limit does not bound fallback work.
- `crates/st3/src/store/runtime.rs` delegates replay to
  `store.rs::replay_graph_from_nothing_with_progress_tx`. That function clears eight graph
  tables and rebuilds operations, base claims, mission runs, planning, resources, glass heads,
  directive notes, custom projections, arrangements and publication facts. The eight-table
  clear list is not a complete generation inventory.
- `WriterConnection::write_background` supplies bounded maintenance turns after at most eight
  foreground dispatch turns. It cannot preempt a loan already running. A different loan class
  or #2644's worker admission alone cannot bound this replay.

The existing fallback logging control
`store::tests::full_replay_logs_every_guard_and_incremental_error_once` is a useful branch
fixture. It proves neither receive-level writer bounds nor partial-publication safety.
[#2859](https://github.com/compoundingtech/smalltalk/issues/2859) owns interim guard and writer
hold telemetry. This design adds no capture or telemetry implementation.

## Decision and storage approval

Choose staged generations with one atomic root publication. Reject two shortcuts:

1. Clearing live tables and committing successive rebuild batches exposes missing or mixed
   projections. Yielding between those batches does not make them readable.
2. Building a scratch database and then copying all rows, rebuilding populated indexes,
   or replacing the database file under one final writer merely moves the
   unbounded work to publication. Replacing the database also risks losing concurrently
   admitted claims and local operational state.

The active root and all publishable generations must reside in the **same SQLite database**
as admission. Scratch storage is only a preparation source. SQLite WAL transactions are not
atomic across a set of attached databases; an attached scratch database plus a root write is
not a durable cross-database swap. See [SQLite WAL limitations](https://www.sqlite.org/wal.html).

The present flat tables have no generation selector or complete connection routing contract.
A format-free table-name swap is **unproved**, not shown impossible. The separable disposable
spike in [the source audit](receive-replay-source-audit.md#disposable-table-name-swap-spike)
checks global index names/`INDEXED BY`, ALTER rewrites, foreign keys/triggers/views, statement
caches and populated cleanup. It is UNRUN. If the complete alternative is qualified, reconsider
the storage decision instead of assuming generations are necessary. A quick rename alone is
not a proof of the full publication hold or unchanged query semantics.

The provisional generation design proposes persisted routing, root and journal state. Initial
conversion might name existing flat tables as generation zero without copying every row, but
the complete logical table, trigger, digest and foreign-key inventory must first be closed.
No format-free constant-work swap or safe migration is demonstrated here.

### Existing storage transition and fail-closed owner join

At the pinned baseline, `st-storage-version::STORAGE_VERSION` is 19 and
`smallclaims::store::reject_old_schema` rejects a nonempty database above that supported version.
[#2390](https://github.com/compoundingtech/smalltalk/pull/2390) proposes the existing 19→20
signature-candidate transition and fleet format hold; it is not a landed generation-routing
capability. Join its storage owner and curator's Nathan decision before proposing activation.
Reconcile this baseline with #2390 at exact `94b8e8414fd75eb11f9d5a7e153e0245666a2e03`: its
shared storage constant is already 20. This proposal must join that existing 19→20 owner/hold,
not reserve storage 21; it implements no version change and assumes no generation capability.

The required gate is stronger than rejecting version 20 in an old version-19 binary: every
binary allowed to open an active nonzero generation must understand and enforce its routing
marker before any read, mutation, schema maintenance or recovery. A flat-only version-20 binary
would not fail closed merely because `user_version` remains 20. Its exclusion/refusal must be
proved in the combined owner plan; no existing marker or compatibility guarantee is assumed.
Until that join is resolved, nonzero activation is forbidden. Do not silently introduce another
version or lower a marker to admit an old binary.

Before an approved conversion, the current receive/full-replay path is unchanged. Once activated,
an unavailable or failed stage cannot escape into inline live replay. Disabling a feature flag
after conversion does not make a flat-only binary safe. Downgrade requires an explicitly reviewed
conversion or forward recovery; snapshots, fleet ordering and old-binary refusal are part of the
format decision. Existing projection rows and incident divergence are not repaired by this note.

A bounded resumable rebuild remains an alternative **only** if it receives the same unpublished
namespace and visibility fence. Rebuilding in place while declaring the graph unavailable
would require fencing every read, mutation and effect path and would withhold the last coherent
view for the whole rebuild. That larger availability change is not selected here.

## Root and input certificate

A projection lease pins one fixed identity: store lifetime, projection generation, complete
logical table map, source cut, authority/local-overlay revisions, executable identity,
claim-schema/storage/rules/digest identities and publication time. All tables used by one
response come from that generation and cut. Ordinary active-generation mutations also advance
its root revision and cut atomically. An old identity must never reopen mutable rows at its old
cut: capture owned response rows within the pinned SQL snapshot, or refuse and recapture. A source epoch or cached digest alone is not a
complete input certificate.

The captured input certificate must cover retained claims and batch metadata, canonical
positions and causal links, checkpoint/tombstone coverage, repair and operation state, and any
local input that full replay or post-projection hooks consume. A numeric arrival frontier alone
misses repair, deletion, rank changes and local overlays. Checkpoint capture's persisted epoch
and durable page cursor are useful precedents, but its sealed-set coverage is not automatically
coverage of every replay input. Reuse a primitive only after that stronger coverage is proved.

Capture uses strict indexed pages in short snapshots. Release each live reader before parsing,
canonical ordering, sorting, folding, scratch writes, awaits or writer acquisition. Sort the
complete captured input privately using the existing canonical semantics, including legacy
positions, checkpoint changes and repair ordering. Do not introduce arrival-order replay or
put the legacy correlated position calculation behind a live-page LIMIT and call it bounded.
Large admitted values need bounded fragments and exact hashes; an oversized legacy value that
cannot be captured within the supported contract refuses preparation instead of truncating it.

A stage records the certificate, fixed target, phase/cursor, previous page hash, cumulative
counts/bytes, output manifest and refusal reason. Each scratch page and its cursor commit
atomically. Only complete certified capture can enter the build phase. Every material source
change outside the certificate invalidates it; ordinary later admissions are handled by the
complete tail protocol below. A fixed capture target prevents endless chasing in one phase.

## Proposed client contract

At the baseline, `smallclaims::replication::ReplicationReceiveResponse` contains only `receipt`,
`changed` and `store_index`. There is no emitted projection-pending field. `ApiError::into_response`
currently emits `code`, `message` and `details`; unknown store codes are normally mapped to 422.
The following shapes are proposed contract changes, not current behavior or generated models.

For a read with no certified root, or an authority-dependent mutation refused **before admission**,
propose HTTP 503 with the existing error envelope and this bounded value:

```json
{
  "code": "projection-not-ready",
  "message": "the certified projection is not ready",
  "details": {
    "retryable": true,
    "retry_after_ms": 1000,
    "applied": false,
    "admission": "not-admitted",
    "stage_id": "opaque-stage-id"
  }
}
```

`stage_id` is bounded metadata, not authority or an input oracle. Preserve the transport's
request/snapshot metadata. Add an explicit 503 mapping rather than inheriting 422 accidentally.
Propose `Retry-After: 1` plus the bounded `retry_after_ms` hint (500–5,000 ms) to prevent
a busy request loop. Clients wait for the hinted minimum or a qualified stage signal under
their bounded wait policy and repeat the **same idempotency key**; no fresh retry operation is
inferred from a pending stage. Unchanged/progress signals do not bypass the retry minimum;
only a certified publication or explicit refusal ends the pending wait. A read must
not silently label an old cut as current. A mutation already admitted cannot return this
`applied=false` refusal: retain/query its operation receipt and report pending completion;
dropping the waiter neither cancels it nor grants a fresh attempt.

For receive, propose an optional `projection` member alongside the unchanged receipt fields:

```json
{
  "receipt": "existing typed receipt, unchanged",
  "changed": false,
  "store_index": 0,
  "projection": { "state": "pending", "stage_id": "opaque-stage-id", "retry_after_ms": 1000 }
}
```

This is shape notation; the existing receipt remains its actual typed object. Proposed states
are `pending`, `published` and `refused`; published includes the certified output cut/root,
refused includes a fixed bounded reason, and pending never claims current projection authority.
Admission acknowledgment is truthful in each case. Historical absence means projection status
unknown, **not published**. Older peers/clients that cannot safely decode or represent the added
state stay on the pre-activation capability path or refuse. Gate the proposed new code/state
on the existing comma-separated `x-st3-features` negotiation using a proposed
`projection-pending.v1` feature; the baseline already negotiates custom subjects/conversation
blocks there, and Rust/Swift/TypeScript clients send that header. Relay the declared capability
through the validated peer route; a header is decoding capability, never effect authority.
Without the feature, omit new projection members on truthful legacy receive acknowledgment
and refuse dependent reads/mutations through a reviewed existing unavailable shape. Do not
alias an unavailable projection to database contention or silently send strict old decoders a
new enum. The precise legacy refusal mapping is part of the paired-contract owner review.
Rust, Swift/iOS, TypeScript, CLI/peer relay and paired-contract handling need
owner review, regenerated models and controls for historical absence, every state, invalid values,
admitted-pending and not-admitted retry semantics. Put both contract changes in the same Nathan
question as the proposed persisted generation format; this note implements neither.

## Generation state and concurrency

| State | Live writer work | Visible state |
| --- | --- | --- |
| Requested | Roll back/refuse the incremental replacement and commit bounded request metadata | Previous certified root, or truthful not ready |
| Capturing | Bounded metadata and source leases only | Previous root |
| Building | None; canonical replay and validation use private scratch storage | Previous root |
| Copying | Bounded rows, indexes and page proof into an unpublished namespace | Previous root |
| Catching up | Bounded source journal capture and unpublished output pages | Previous root |
| Sealed | Fixed manifest and current-cut certificate ready for CAS | Previous root |
| Published | One bounded root/frontier/health/digest metadata transaction | Entire new root |
| Aborted or retired | Bounded cleanup pages | Active root remains intact |

Receive commits envelopes and admission receipts before requesting rebuild. Its `pending`
projection result does not undo peer delivery, fabricate projection readiness or claim effect
authority. New rebuild requests coalesce by store lifetime and input identity; one owned
background coordinator carries them. An unrecognized runtime or guard remains an explicit
refusal, never a synchronous full-replay escape hatch.

Local mutations continue under the existing writer admission and FIFO. Each mutation pins and
rechecks its current generation inside its writer transaction and commits its native changes,
projection writes and journal entry together. The journal must include every projection-relevant
admission, repair/trim/canonical change and non-claim local overlay. A timestamp or store-index
counter without that closure cannot certify catch-up.

A safe mutation independent of unavailable derived authority must commit between rebuild pages.
A mutation whose ownership or declaration cannot be certified receives a bounded retryable
not-ready response; it must not authorize from a stale published list. This distinction is
necessary for missing/unhealthy roots: the design cannot promise that all 644 historical work
mutations could safely commit before a rebuild. Tests must demonstrate actual committed
ordinary mutations where authority is valid, and separate refusal where it is not. Merely
incrementing an admission counter or returning early is not the forward-progress control.

The builder applies journal entries after its captured cut to the unpublished generation.
Canonical changes requiring a refold restart private preparation rather than holding a writer
through a refold. Stage identity, target and completed page proofs survive that decision; an
incompatible capture is abandoned, not silently reused. If arrivals outrun catch-up, report
pending progress and bounded disk pressure. Do not solve it by an unbounded admission pause.
Use existing background fairness, releasing every page, rather than overriding priority.

Every read and write that addresses a projected table needs a generation-binding contract.
Existing raw SQL must be audited and routed; installing read-only views is insufficient for
writers, foreign keys, custom reducers, triggers, digest maintenance and operational overlays.
The [counted source audit and demonstrated non-claim inputs](receive-replay-source-audit.md)
include those dependencies and all `after_projection` work; dynamic routing and journal closure
remain unqualified.
No root can be sealed while any hook or logical table remains outside that inventory.

## Bounded publication and truthful reads

Copy scratch output into a hidden generation with indexes created empty before row copying.
Maintain its logical row hashes and per-table accumulators with each bounded page. Building an
index over a populated table, copying an entire table, global DELETE, VACUUM and automatic
inventory reconstruction are forbidden inside a receive-owned publication hold.

At publication, an owned writer transaction validates expected active root, store lifetime,
source/authority revisions, complete journal watermark, executable and storage/rules identities, and the fixed-size sealed
output certificate (completed cursor, cumulative counts/hashes and output mutation epoch).
The certificate is established by all preceding bounded page commits and private validation;
publication must not iterate an unbounded list of page manifests. Gaps or a changed output epoch
refuse publication. A changed predicate refuses the swap
without altering active rows or health. Catch-up continues outside the writer; a short seal
window may exclude new admission for **one bounded final page** only. It cannot wait for work
or drain an arbitrary backlog while retaining the writer.

The final commit updates the active root, complete table/digest manifest, truthful projection
frontier, fixed publication time and health together. Metadata has a fixed inventory/byte cap;
if it does not fit, sealing refuses. It performs no row copy, full digest scan, reducer or cache
construction. Existing bounded transaction finalizers and native committed-authority observers
remain part of the native commit boundary: committed index updates precede synchronous observers,
which finish before success acknowledgment; lent observers finish inside guard drop before the
connection returns. Observer panic refuses acknowledgment and stops the writer. Charge their
work and cleanup to the full acquired-to-returned hold and retain the parent permit until real
callback/guard completion. Only deferred publication, subscription and effect notifications run
after physical writer return. Caches and subscription frames carry the generation/cut; an old
cache cannot label new source metadata as the same view. Current effect guards remain fresh and
outside pinned read snapshots.

A reader pins a certified root in its short SQL snapshot. It returns that view's own cut/time,
which can precede the latest native admission. If none is qualified, it returns truthful retryable
not ready and wakes the coordinator. Reads never build, fold or write. Existing snapshots may
finish against the old generation; garbage collection cannot remove it until its actual leases
end. No SQL guard or strong store is retained across network waits.

## Proposed work and time limits

These are implementation acceptance ceilings to qualify, not measured performance or an
absolute guarantee for failed hardware. Admission waiting and commit/rollback time must be
reported separately from SQL execution, and the full acquired-to-returned writer hold is the
quantity under test.

| Live phase | Work cap per transaction | Time budget |
| --- | --- | --- |
| Request and fence | At most 16 keyed metadata operations, 32 KiB owned metadata | 10 ms SQL budget |
| Copy, journal application or cleanup | At most 64 input/output rows, 256 KiB total encoded owned payload plus metadata, 128 bounded point references | 25 ms SQL budget |
| Final seal and publication | Fixed registered table manifest, at most 32 KiB metadata and one bounded tail page within the previous cap | 25 ms SQL budget |

All phases release the managed writer between pages. Charge expanded trigger, cascade, index
and reference work to the same page budget; 64 top-level rows cannot hide unbounded fanout. Statement/VM budgets and an owned monotonic
deadline must interrupt a page that cannot fit; rollback and owner exit must finish before reuse.
Do not replace another owner's SQLite hooks or pretend a catchable error poisons a writer.
Uncertain rollback requires actual managed-owner quarantine. This boundary must be qualified,
including physical writer return; a row limit or statement count alone proves neither elapsed
work nor cleanup. Fsync, SQLite automatic checkpointing, lock waits and rollback can exceed a
SQL timer. Preserve durability and measure those costs; do not disable them to pass a test.
A hold exceeding the qualified ceiling is retained as a failure, not hidden by retry.

Bound scratch/candidate disk and retained-input bytes with an explicit owner quota before
capture, cap per-page memory, and make WAL growth part of admission accounting. Exhaustion
ends or pauses preparation with an actionable reason and keeps the old root. No default disk
quota is selected without measured store sizes and storage-owner review. Account for scratch,
active/hidden/retired projections, retained inputs and WAL together. ZFS snapshots can retain
retired blocks after SQLite deletion; cleanup completion does not prove byte reclamation.
Coordinate the combined reserve with [#2849 sorter/read spill](https://github.com/compoundingtech/smalltalk/issues/2849)
and the storage owner. Old generations are
removed by resumable bounded key-range deletes, then a bounded empty-table retirement; DROP of
a populated generation must not become the next long writer loan.

## Restart and abort

Crash before publication leaves the old active root. Resume a scratch/copy cursor only when its
complete identity and page proofs match; otherwise mark the stage abandoned and clean it in
bounded pages. Crash during the same-database root transaction leaves either the old or new
complete root. Crash after commit but before notification recovers the published root and emits
an idempotent generation notification without repeating the swap. No startup full-row scan is
required to discover which root committed.

Each stage has a current owner/lease, non-blocking single-coordinator lock and cancellation
identity. Supersession, incompatible build/schema/rules, store replacement, lost input retention,
manual cancellation and output corruption refuse its successor. Cleanup may never reclaim the
active generation. Resume compatibility and the ability to open old committed roots across
builds need explicit migration and rollback policy. An old binary that only understands flat
tables cannot be assumed to open this format.

## Controls required before implementation lands

Use disposable stores, the actual receive handler and managed writer boundary after heal removal.
For each branch below, grow unrelated history and use a competing authorized mutation. Pin the
compiled/tested source and full replay reference inputs; retain initial failures, built-in retries,
terminal outcomes, raw hold/VM/byte tuples and cleanup receipts separately.

| Trigger or boundary | Required discriminator |
| --- | --- |
| Missing health | No certified root: not ready until publication; safe independent mutation commits between holds |
| Unhealthy projection | Retain prior certified root only if its own lifetime/cut is still valid; damaged replacement never becomes readable |
| Frontier ahead | Fence refuses impossible frontier; capture authoritative retained source rather than publishing the erroneous index |
| Non-incremental kind | Canonical private replay preserves the existing supported/quarantine semantics; no arrival-order shortcut |
| Malformed operation | Exact malformed input is handled as by full replay; no partial operation/receipt and no silent success |
| Operation conflict and ReplayWithContext | Preserve exact conflict context; retain the existing incrementally repaired message-send exception |
| Plain Replay result | The runtime request goes through staging; no guard may directly invoke live full replay |
| Non-contention incremental error | Roll back the failed page, retain error identity, then stage; no partial incremental rows survive |
| SQLite contention at begin or during projection | Defer without full replay or copied success; the current typed contention semantics remain |
| Empty receive after catch-up deferral | Rebuild advances without fresh envelopes and without a live full-replay loan |
| Concurrent admission, repair, trim, rank or local overlay | Complete journal/fence either incorporates it or refuses CAS; no lost native write or fabricated authority |
| Every stage and publication crash point | Old or complete new root after restart; no mixed rows, repeated effect, erased receipt or orphan owner |
| Digest and reducer closure | Typed logical rows and independent per-table oracle equal a full replay at the SAME certified cut, including operations and post-hooks |
| Reader, cache and effect races | Old reader remains coherent; new reader sees complete root; effects use fresh authority; silence and unchanged native input still check lifetime |
| Limits, disk pressure and cancellation | Bounded pages/refusal, actual writer/delegated cleanup, fair foreground progress, no inline fallback |
| Admission dependency and lost wake | Current-thread and multithread parent/child permit sharing; 32 parked non-writing callbacks still let a 33rd mutation commit; wake in the Pending-to-wait gap retained |
| Request disappearance | Dropping HTTP waiter preserves admitted mutation; pre-admission cancellation submits no callback; started child retains permit until physical completion/panic cleanup |
| Staged proof and final acknowledgment | Scratch plan/proof finishes with live writer held and has no live-writer admission; acknowledgment follows the actual fenced native swap commit, retained result, finalizers and native authority observers; observer panic refuses acknowledgment |

The full replay oracle belongs only to isolated qualification. It may not run as a live production
read or final swap check. Cached digest equality alone is insufficient; the fixture also compares
rows and certified input. Include repaired/checkpointed histories, restarts, duplicate receives,
late envelopes, conflicting operation identities, custom/resource inventories and large legacy
values. Increasing N must grow total preparation pages, not the largest writer hold.

## Coordination and landing

[#2837](https://github.com/compoundingtech/smalltalk/pull/2837) changes only a new declarative
origin write to use its proposal's immutable acceptance time. It changes neither replay order
nor this staging contract. Its mixed-build and unrepaired-existing-state obligations remain;
staged replay is not permission to perform incident repair. No decision on the separate terminal
timestamp candidate is imported here.

[#2644](https://github.com/compoundingtech/smalltalk/pull/2644) is an unlanded admission candidate,
not a current-source capability certificate. Its owner supplied the following source contract at
`db44d827faf34aa0e88cf6bb92f60532ee0eca0a`:

- Capacity is per `WriterConnection`, default 32 top-level callback chains admitted before
  `spawn_blocking`. Dependent same-writer children capture and share the parent's Arc-owned
  permit; independent stage producers must not inherit it. Waiting for new admission holds no
  store, reader or cache lock. There is no new global blocking-thread cap.
- Each synchronous mutation-future poll owns admission; Pending returns its worker/capacity
  before awaiting the retained Notify wake. Restore handler/store/read-budget/profile/CPU and
  writer-wait scopes on every poll. A dependent child retains the shared permit until it really
  completes. No entire handler, body, peer/control round trip or convergence wait holds a slot.
- HTTP waiter disappearance does not cancel an admitted mutation. Canceling a fresh admission
  before a callback starts submits nothing; canceling its waiter afterward cannot free capacity
  or fabricate completion. A stage's explicit owner cancellation is separate from that waiter.
- Existing checkpoint capture is admitted via `blocking_store`, then the SAME owned staged set
  goes to planning and scratch proof via `blocking_external`, without borrowing the live writer.
  Preserve its fenced cut, staging lock, success/error cleanup, response and outer allocator
  completion. Scratch preparation of this design follows that separation; copy/seal pages return
  to native admission and the real transaction/finalizer boundary.
- Commit acknowledgment follows native commit and retained result, not scheduling, yielding,
  canceled request or finished scratch proof. Final publication must revalidate the exact cut.

Source references: [writer admission](https://github.com/compoundingtech/smalltalk/blob/db44d827faf34aa0e88cf6bb92f60532ee0eca0a/crates/smallclaims/src/sqlite/writer_admission.rs),
[mutation polling](https://github.com/compoundingtech/smalltalk/blob/db44d827faf34aa0e88cf6bb92f60532ee0eca0a/crates/st3/src/api/read_deadline.rs), and
[admission liveness](https://github.com/compoundingtech/smalltalk/blob/db44d827faf34aa0e88cf6bb92f60532ee0eca0a/docs/st3/writer-admission-liveness.md).
The owner has no overlapping replay/format edit and supplied no native or implementation GO.
Its required Linux gate negative remains adverse; isolated passes do not establish causal
independence. Bounding replay is independent of #2644 landing; worker admission is not a
prerequisite workaround for the live writer defect. This design does not waive that result or
adopt the candidate as a landed API.

[#2859](https://github.com/compoundingtech/smalltalk/issues/2859) telemetry must land before
implementation qualification, retaining the named guard and full acquired-to-returned hold.
Telemetry alone neither diagnoses the reported request nor bounds replay.

Intake reviews this design and the complete generation/input inventory first. F1/F3 experiment
and interleaving results remain absent; the source recipes are dormant review inputs. F1 results,
the routing classification and the storage 20 owner reconciliation precede the Nathan question;
F3 results instead gate implementation landing. Nathan routing stays held until that required
decision evidence exists. A docs-only proposal may land
without approving its storage or wire changes. The curator routes
the persisted-generation recommendation and any format hold to Nathan. Only then can an
implementation mission choose exact format/rules transitions, limits, conversion, mixed-build
ordering, rollback and the control run. This note does not change runtime code, claim shapes,
schema, replay, telemetry or production state; no implementation or rollout date is claimed.
