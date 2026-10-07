# Application-shaped incremental-view fixtures

All 23 prototype tests have passed in local iteration. These are restricted fixture proofs;
the runtime has not been activated in production.
Run the integration cohort with:

```sh
cargo test --locked -p smallclaims --test ivm_real_views -- --nocapture
```

The fixtures use real Store append transactions and receive, validate and project
replication phases. Independent raw-history folds compare outputs after commits.
Envelope delivery is forward, reverse or rotated, with duplicates. When at least
three envelopes can be permuted, the fixture asserts distinct orders. Canonical
keys are captured on each target by its projection runtime. Separate tests cover
rollback, persisted outputs, readiness and semantic invalidations.

## Relations

| Family | Fixture relation and limits |
|---|---|
| Agent card | Runtime incarnation; harness state and independent boolean auth evidence; maximum cumulative total; canonical step ownership and old/new actor membership. Omitted auth cannot erase a refusal, and restoration can change permission without changing work. Full operational card, lease/deadline/prompt/presence predicate, work queue, cost and response-delta aggregation are absent. |
| Account limits | Highest weekly reading inside the inclusive hour behind the freshest original weekly sample, greatest reset first, then percent/time/seat/canonical ties. Partial five-hour evidence cannot freshen weekly evidence. Identified provider accounts are partitioned by driver/account/account-ref. Full declared-label normalization, privacy, bindings, active seats and stop policy are absent. The historical comparator comes from the application's limits reader. |
| Fleet | Direct listening-mode admissions and anchor removals by a pinned anchor, checked against the crate's membership fold. Signers come from verified envelope storage or the configured local node key, never claim JSON. Admission floors are zero (including the omitted anchor default), root never ends, and there is one admission per member. Bootstrap precedes child permutations; child removal can precede admission. General sponsor chains, writer windows, conflicts, cycles and complete authority admission are absent. Unsupported sponsor/mode/window/remover or unproven signer input fences readiness. Signature mutations invalidate affected fixture state; an anticipated local signature is equal evidence. |
| Mailbox | Current sent rows, terminal read/closed membership and per-recipient unread deltas, including read-before-send and recipient replacement. Full sender/recipient authority is an application admission prerequisite, not supplied by this claims-only fixture runtime. Thread/tag/answer/reopen/age and cursor semantics are absent. |
| Mission tree | Indexed run/step rows, nested run parent and canonical step state. Immutable layout is installed as bounded local-source input on an empty fixture and supplied independently to the oracle. Authored-definition validation, generation ownership, gates and queue rank are absent. |
| Desired by host | Unowned canonical declarations, stops and prior-host candidates. Late prior-host membership advances the subject's semantic feed even with unchanged selected row. Non-null set/run/step/generation ownership fences readiness. Typed revisions, owned lineage, omission, sibling blockers, accepted repair, adoption and effect authorization are absent. |

The claims-only ViewRuntime accepts schema inputs like Plain. Production must keep
its schema and authority admission before calling generic Views. Invalid or
unsupported records must not become admitted view inputs. A prototype ready token
under these restricted inputs cannot certify a production reader with additional
dependencies. Candidate helpers are diagnostic until readiness, authority and
output are captured in the same snapshot.

Maintenance failure preserves source admission and successful other views in the
transaction while rolling back the failed view's savepoint and fencing it. The
shared source cut can advance, but the failed view keeps deferred evidence rather
than an applied ready cut. Effects requiring it must stop. These unscoped fixture outputs have no recovery
installer; namespace-aware installation is a separate opt-in interface. There is no
implicit read/open/repair/checkpoint replay. The prototype rejects
its checkpoint sealing, planner, copy proof, application, interrupted resumption, direct trim,
adoption and checkpoint-history restoration before their history/copy/mutation work;
local derived tables are excluded from shared projection digests. Targeted negative
controls check persisted rows, generations, frontiers, action lists and scratch files.
Low-level SQL helpers are not protected by this runtime entry-point check.
General explicit consistent backup remains available. A raw copy is not a ready
restore/install artifact or a checkpoint proof. This is not a checkpoint retention,
adoption or mixed-build compatibility contract.

The prototype requires exclusive ownership of its file by the view runtime. A
different runtime on the same file can bypass runtime-only checkpoint guards;
there is no durable cross-handle registration fence. Raw public tombstone record,
delete and keep-only helpers and Store.connection are also outside this guard.
Diagnostic and human excusal appends remain available and do not prove a checkpoint.
Production must enforce database ownership/capability fencing before wiring.

This unsupported prototype cannot participate in a production checkpoint fleet:
it neither seals/verifies nor adopts a peer's trim. Counting it as a participant
would prevent stability; excusing it would not make adoption valid. A reviewed
participation/retention/install and mixed-build capability contract is required
before deployment. There is no automatic excusal, silent checkpoint skip or
success-looking lifecycle fallback. Manifest need refuses before the sync worker
fetches a peer manifest. A certified positive-flow comparison remains a gap; raw
interrupted metadata controls are not such a certificate or execution proof.

## Historical observations and current inputs

Historical limits retains candidates and radix summaries; work selection retains
candidate claims. Growth is unbounded until an explicit retention contract exists.
No writer budget or growth result is qualified. Range lookup uses fixed-width
aligned blocks and summary updates stop when an ancestor is unchanged. Canonical
legacy batch positions still use existing COUNT semantics, not arrival order.
Unsupported same-index rank changes can fence the prototype.

Latest per-seat state is not equivalent to historical limits: replacing a reading
can discard an earlier higher weekly sample still inside the hour. These fixtures
do not propose production telemetry claims or reconstruct telemetry history.
Current adapters need separate old/new seat, incarnation, host and account tests;
replacement/removal; independent weekly/five-hour slots; original time; reset
advancement; stale exhaustion; and incomplete-evidence readiness.

## Correctness instrumentation and future measurement

Noise tests install mutation-witness triggers on projection tables, so token
equality cannot hide unchanged-value rewrites. Unknown kinds may advance shared
availability, but must touch zero tracked projection rows. Witness triggers are
correctness instrumentation and must be omitted from a cost unit. History oracles
and diagnostic row counts also stay outside timing.

With test-support, sqlite::work::total() exposes process-global statement, VM,
fullscan, sort and autoindex counters for isolated serialized measurements.
Changes separates affected, changed and deferred keys. Physical growth is separate
from semantic generation. SQLite profile or commit time is not writer-held time;
measure acquisition through release and queue wait. These correctness tests supply no writer-cost result.

## Migration batches

1. Review and execute the mechanism/examples, port arrangements with exact
   vocabulary/authority/cycle parity, and implement explicitly initiated bounded,
   crash-resumable installation with atomic ready publication. Decide checkpoint,
   layout, retention and mixed-build compatibility before production wiring.
2. Migrate durable mailbox point/list/count and mission run/step point/tree readers
   together with complete admission, definition and generation dependencies.
3. Migrate current card, attention and predicate/wait consumers with complete
   subject/actor/current-source dependencies, independent availability, clock
   watermarks and commit notifications. Numeric policy needs a complete source
   selection and coverage contract.
4. Migrate fleet authority, owned desired sets and diagnostic evidence after
   signer/window/cycle closure, historical omissions, sibling invalidation and
   accepted repair/retraction have independent source and execution proofs.
5. Migrate private search with persisted source identity, authorization, index
   format and bounded cold/reopen lifecycle; registers alone cannot populate it.

Each batch requires review, execution, growth and lifecycle proofs, then a reviewed
merge before adoption. Page continuation, availability invalidation and durable
transition replay are distinct contracts; a coalesced feed is not an action or
edge-event witness.
