# Admitted input and applied output: next bounded slice

Status: first reviewable source slice, 2026-10-09, following plan `938933bcb`.
The additive fixture and narrow input-budget method are authored. Current
successor controls are UNRUN; earlier hosted outcomes stay at their tested heads. No local compiler, SQLite execution, load, collector, production
registration or activation occurred. The asynchronous-catch-up claim remains open.
The first hosted test producer on `59ee523dc` failed before any selected control:
the native savepoint fixture passed a Savepoint where Runtime requires Transaction.
Its correction uses an explicit nested savepoint on that managed outer transaction;
failure-qualified cleanup remains a separate prerequisite, not a waived failure.
At `442d6365a`, the hosted producer compiled and selected all fifteen names on
merge `68246f099b7fea81462e4f134576e29677fc7b7c` (main `4f0e4787e`).
Fourteen substantive controls failed setup because the fixture rejected Store's
native TEMP write_clock; only the no-phase child entry passed. The accounting
control did not reach measurement. Clippy separately rejected unused PENDING_BYTES.
This fixture-only correction permits that unrelated TEMP table name/type while
refusing every other TEMP object, including Installer/native/output shadows and
TEMP triggers attached to main. A new control checks refusal before output/P DML,
then publication after fresh validation. The quota SQL now uses PENDING_BYTES
instead of the identical 2 MiB literal; limits and assertions are unchanged.
At `4fb590855`, hosted merge `c2697667b5c6e57cfefbdce270c8b4b93f459ecb`
(main `86ed5b7b`) compiled, passed Clippy and selected all sixteen names.
Fifteen passed, including native crash boundaries and TEMP negatives. The strict
accounting control failed all three existing CI attempts at 273 statements,
6,474 VM steps, 672 fullscan steps, two sorts and zero autoindex rows. Its 128
statement ceiling is unchanged. The current source shares one bounded main
foreign-key inventory across the requested output tables through additive
capture_tables; single capture_table is its one-table wrapper. Per-table shape
and fanout refusals remain, and metadata acceptance is atomic across the group.
New controls cover exact/one-over 256 tables and 128 FK rows, incoming/outgoing
and composite/case-variant edges, main FK visibility under a TEMP shadow,
single/group publication parity and unsupported second-table refusal. Current
successor work counts and controls are UNRUN; this is not a measured improvement.
The separate optional cost job failed link-minutes delivery/read/receipt evidence;
no cause is attributed to this dormant source or the metadata walk.
The first grouped source `ce5ccfb7e` failed its hosted producer before control
execution: the new malformed-metadata refusal used an unimported bail macro.
The macro correction explicitly qualified anyhow::bail without traversal changes.
At `fcc64d252`, hosted producer and Clippy passed and all 23 names ran: 22
passed, including all seven grouped controls and substantive crash boundaries.
The strict page control failed all three automatic attempts at 135 statements,
5,411 VM steps, 549 fullscan steps, one sort and zero autoindex rows; the unchanged
128-statement ceiling still refuses qualification. Own logs are retained under
`/var/tmp/ivm-admitted-applied-slice-ci-fcc` (jobs 114055848153 and 114055848008).
The next source consolidates requested table shapes into one main-bound metadata
statement with a materialized table-kind/header inventory, explicit main xinfo,
trigger refusal and per-table 128-column/global lookahead bounds. Request/union
bounds still precede discovery; schema is checked before and after the entire
capture; FK traversal and all-or-nothing acceptance remain unchanged. NOT INDEXED
prevents an automatic header join index. Two additional controls cover 128/129
columns, main shape discovery under TEMP shadows, generated and missing siblings.
All 25 successor controls and work counts are UNRUN; row bounds and static query
consolidation are not an engine-work or production adoption certificate.

## Source and actual dependencies

The base is main `5d26e286ed4443d5f1fe5b68dc31788f5a867ad9`, fetched after
an exact remote-main read. Its Installer, prepared publication, Core Store and
SQLite finalizer files are unchanged from inspected main `5c396ef2a`.

Existing mechanisms to exercise are `Installer::register_source`,
`enable_deferred`, `capture_deferred`, `start`, `prepare_scan`,
`prepare_catch_up`, `prepare_live`, `publish_prepared`, `status`, `root`,
`prune_journal` and explicit namespace reclamation. The fixture uses these on a
file-backed **smallclaims::Store**, through its existing reader/background writer
loans, `Plain` native Runtime and explicit fixture triggers. It does not substitute
a bare Connection fixture or edit a production Runtime.
The earlier `ivm_prepared.rs` controls establish a useful fixture shape but
are not an execution certificate for this slice.

PR #2143 supplies the current hosted dependency for this source slice. #1981 closed unmerged
with no successor. Its former reactor owner and private source callback APIs
are historical, not an installed current-main provider. PRIVATE writer `78`,
checked reader `91`, and retained publication `d3ed` remain unadopted; their
controls and missing integration obligations do not become passes here.

## One deliberately narrow source

The first source is a claim-ID ledger in an isolated fixture. Each newly
admitted native claim contributes exactly one output row containing its ID,
subject, kind and an off-writer digest of its bounded raw body. All kinds,
including an unknown kind, contribute; there is no kind-specific admission
change. This proves an admitted/applied prefix, not canonical selected-parent,
run-tree, card, thread, Doctor, fleet or authority coverage.

The source is installed explicitly for a new empty fixture lifetime. A fixed
fixture trigger descriptor captures the stated claim columns, using the existing
Installer revision/journal, not a second revision counter or scheduler.
An immutable image is retained at each captured revision in the same native
transaction. An image includes the bounded source facts needed for output;
it cannot be reconstructed later from a mutable current row. CapturePlan uses
the actual full claims PRIMARY KEY, `store_index`; `id` is UNIQUE, not the PK.
The immutable image binds that native index to the claim-ID output key. Image
bytes include the fixed eight-byte native index. Repeated ID admission via
OR REPLACE fences before copying rather than conflicting with a retained image.
The pre-copy fence covers both unique image keys: claim ID and store_index.

INSERT admission and its reference/image either commit together or roll back
together. Duplicate native admission must produce neither a second reference
nor a second output. Raw source UPDATE/DELETE, rekeying, repair, checkpoint
removal, incompatible DDL or unsupported input fence this first source in the
same transaction. They do not silently leave a ready ledger. Incremental
repair/retraction is outside this slice; explicit unavailable is its result.
Native claims, admission checks, signatures, SEND and receipts are unchanged.

Coverage is an exact source name/fingerprint/lifetime/epoch and contiguous
Installer revision prefix. The admitted revision A and applied revision P
are read with output in one SQL cut. Current readiness requires available,
the expected identity, validated metadata, P=A, and no pending/gap evidence.
When P<A, the output is pending and must not be returned as complete current
output. No historical namespace capability or stale-cut consumer is offered.

## Page protocol and atomicity

1. Capture at most one contiguous indexed journal page and its immutable
   images in one short supplied read transaction. Check identity, predecessor
   revision, widths and quotas before decoding/copying each value. A missing
   image/reference, incompatible schema or discontinuity refuses the page.
   No full claim scan, canonical COUNT, JSON history fold or prefix rescan.
2. Close the owned read successfully before digesting or constructing writes.
   Do not return a connection, statement or reader guard with the owned page.
   No network wait or asynchronous wait may retain its snapshot. The fixture
   must exercise actual reader return; legacy lexical callback return alone
   is not a checked-exit receipt.
3. Compute at most one fixed-width output per image off the writer. Build
   point writes plus explicit completeness/coverage checks in PreparedPage.
4. Acquire the existing background writer loan. Recheck lifetime, schema,
   job/root identity and expected applied prefix after acquisition. Execute
   precomputed bounded writes only. Output, P, pending counters and coverage
   are changed in the **same outer native transaction**. Publish no process
   Ready or completion ACK before its actual COMMIT succeeds.
5. Return the writer after this one page before the next acquisition. A native
   foreground append must interleave between pages. New admission during
   preparation remains pending; publishing an older contiguous page cannot
   relabel it as having covered that newer admission.

Replaying an already committed page is an explicit superseded/no-op refusal,
not a second generation increment. Crash recovery uses the committed prefix
and indexed next revision, never a cached process cursor or a replay from zero.

## Proposed limits to enforce and test

Rows/bytes/fanout guards and boundary controls await successful successor execution.
Statement/VM ceilings remain an unexecuted accounting prerequisite; current
metadata discovery must not be called qualified on result limits alone.

| Resource | Limit |
| --- | --- |
| Sources / consumers / active jobs | 1 / 1 / 1 |
| Native image, including copied identity fields | 8 KiB; key <=128 bytes |
| Journal reference, serialized bytes | 512 bytes |
| Retained pending images / image bytes | 256 / 2 MiB |
| Retained journal rows / bytes | 256 / 128 KiB |
| Input images per page / copied page bytes | 16 / 128 KiB |
| Output fanout | 1 row per image; no reverse dependency walk |
| Output row / prepared writes and checks bytes | 512 bytes / 32 KiB |
| Publication table registry | output and coverage only, 2 tables |
| Page data writes | <=16 output point writes plus one coverage update |
| SQL statements / SQLite VM steps per page | proposed <=128 / 50,000, including trigger work |
| Lifetime output keys / cumulative consumed references | 4,096 / 4,096 |
| Journal/image reclamation page | <=16 rows and <=128 KiB, indexed |
| Namespace reclamation page | <=16 rows across all owned tables |

`prepare_live_bounded` separates journal input row/byte limits from the
PublicationLimits combined input/write/evidence budget. Original `prepare_live`
uses its identical prior limits through the shared inner helper. This narrow
additive accounting delta needs exact independent review. Metadata discovery
work is not certified by that budget. Statement/VM ceilings require actual enumeration
and appropriate existing budget ownership. A sampled post-callback duration
does not enforce them. SQLite serial-type/direct-column width eligibility,
index plans and metadata cost must be proved before calling the page bounded.
No arbitrary JSON parsing runs in capture triggers. A length test that itself
materializes a huge value, or eager AND before JSON parsing, is insufficient.

Quota/width/VM/decode/fanout/lifetime exhaustion makes source coverage
unavailable or stops the job without partial publication. Oversized admitted
native input is not dropped or rejected solely to preserve this experimental
projection. The fixture source must establish how to fence without an
unbounded body copy; this is an implementation obligation, not assumed here.

No image/reference may be pruned above the sole consumer's committed P.
Pruning counters and deletions commit together. Stop/source refusal keeps the
source unavailable and retains required input until explicit bounded cleanup.
Cancellation refuses the operation without advancing P; it does not invent a
durable source fence or delete input.
No unbounded shadow namespaces, retention scan or cleanup on GET/admission.
No retained parent graph is needed for this one-output-per-input slice.

## Real Store control obligations

The authored test file is
`crates/smallclaims/tests/ivm_admitted_applied_slice.rs`, with support under
`tests/support/ivm_admitted_applied_slice.rs` only if needed. It uses private
temp directories, file/WAL Stores and normal writer ownership. No shared host
process, daemon, live database or production admission is touched.

1. **Native rollback:** append through the Runtime in a managed Store outer
   transaction, then roll it back. Native claim, immutable image, reference,
   admitted evidence and output must all remain absent. Test an intervening
   savepoint rollback and a duplicate as well.
2. **Atomic page rollback/commit refusal:** stage a page, fail after the first
   output write and before P/coverage publication, then separately refuse the
   outer COMMIT. Compare all output, frontier and quota tuples before/after;
   verify the reader still refuses incomplete current output. Retry the exact
   committed input once with no duplicated output/generation.
3. **Actual crash/resume:** child processes own the private Store. Exit after
   admitted COMMIT but before application; reopen and resume from persisted P.
   Exit during an uncommitted output page; reopen and observe complete rollback.
   Exit after output+P COMMIT but before process notification; reopen without
   reapplying that page. Record handshakes and child exit mapping, not a claimed
   in-memory simulated crash.
4. **Newer input and interleaving:** admit another claim while an owned page is
   computed; a foreground native append runs between two background pages.
   After the first publish, P covers only its original page and current Ready
   remains false. No borrowed SQLite object escapes preparation.
5. **Source lifetime:** same-file compatible restart resumes only after bounded
   schema/source validation. Wrong fingerprint/epoch, replaced file, copied
   fixture file and changed schema remain unavailable; an old prepared page
   cannot publish. A fixture-owned lifetime manifest and file identity are
   independent of copied database metadata. They are not a production
   incarnation/restore certificate. Arbitrary live in-place replacement has
   no supported contract and remains a production blocker.
6. **Gap and unsupported mutations:** missing image, deleted/changed reference,
   out-of-order revision, raw claim repair/delete/rekey and unsupported width
   all refuse readiness; never invent a zero revision or omit an input.
7. **Bounds and retention:** cap/cap+1 rows, bytes and fanout; duplicate/unknown
   kind; cancellation; SQL/VM/decode stop; pruning across P; quota rollback;
   stopped-job explicit cleanup. Assert actual indexed work/copy counts, not
   just returned rows. No skip-and-continue across an oversized image.
8. **Failure-qualified ownership:** reader rollback failure, prepared-page
   savepoint release/cleanup failure, hook panic and an old transaction handle
   after rollback must not allow a sibling native commit or reuse an uncertain
   connection. These require the separately owned writer/reader contracts;
   absent adaptation is a failing prerequisite, not a test allowance. A caught
   anyhow error or rusqlite Drop is not quarantine.

The first fixture oracle compares every admitted ledger fact with a simple
synchronous reference set at the same cut. It explicitly does not claim
application-shaped canonical-view parity, native authority completeness or
50 ms commit-inclusive writer latency. Those remain subsequent work.

## Exact reservation boundary and next source change

This commit reserves the new plan and the proposed additive test/support files.
For the implementation review, Foundation also retains narrow ownership of
`install/prepared.rs` capture/prepare/publish/savepoint-exit and quota-accounting
helpers, and `install.rs` source position/gap and bounded journal/reclaim
helpers. Changes there need exact review; this plan does not authorize a broad
rewrite. Existing CapturePlan/main binding, events, retained capabilities,
SQLite writer abort/finalizer and checked-reader reservations stay Foundation's.

No reservation is taken on append_claim_record_tx, next_replica_sequence,
previous_batch_hash or the proposed batch-head helper; reconciliation's two
post-success commit counter lines stay its owner’s. No st3 Runtime/constructor,
reactor, route, thread candidate, card or health-source hunk is reserved.

The first source slice is the additive real-Store fixture and bounded-input
page adapter above, ready for exact review against this pinned base. Fifteen
normal tests (including the crash subprocess entry and TEMP-boundary control)
and one named work-accounting prerequisite are authored. Current successor
outcomes remain UNRUN; the failed 442 cohort is recorded above. With test-support that prerequisite requires
positive traced statements and unchanged 128-statement/50,000-VM ceilings;
without test-support its same-name twin fails explicitly rather than disappearing.
The byte-bound control admits exactly one fitting reference while leaving the
next individually small reference pending, refuses a one-byte-short bound,
and checks the original selection/combined accounting. The in-turn fault control
uses fixture-owned hooks with an outside-SQL atomic witness of actual output DML,
then refuses coverage insertion before P; rollback and exact retry are checked.
The fixture physical read owner
refuses inherited pinned readers, checks idle acquisition/exit, removes
cancellation before ROLLBACK, discards its exact reader on failed exit and
returns clean errors/panics only after exit. This is private fixture logic, not
Core checked-reader API adoption. The external lifetime manifest is bounded;
it is not a production early-open/restore guard or a synced crash-safe manifest
replacement protocol. Arbitrary live in-place replacement remains unsupported.

No writer savepoint/finalizer/guard correction was made or hidden: fatal writer
cleanup, cancellation/work enforcement, incompatible physical schema, raw cookie
reset, complete trigger/repair closure and general consumer read bounds remain
prerequisites. Cooperative budget checks cover capture, off-reader reduction and
publication, with cancellation preserving the unapplied durable prefix; these
do not qualify a VM/hold-time bound or fatal writer cleanup. The thread-local
work-accounting scope covers traced preparation/publication and physical guard
return, excluding earlier admission and work on other threads. Admission trigger
cost and commit-inclusive hold/queue/CPU remain unmeasured. The strict
work-accounting test may expose a needed metadata-cost
correction; its assertions must not be waived or weakened. Native writer startup,
supervisor/quarantine and actual work questions remain explicit before adoption.
Current PR checks supply the live dependency; no historical callback is invented.


## Composed-main accounting successor

The PR-head 1b67 cohort passed all25 names and unchanged strict accounting, but
merge-group38019093612 (dc4d1abe, main plus2154 plus1b67) failed the strict control
on all3automatic attempts at129statements/6350VM/642fullscan/1sort/0autoindex.
The new main schema adds checkpoint_capture_epoch; the whole-inventory FK walk
now probes this additional table. This is a source-supported explanation, not a
fabricated per-statement execution witness. The composed failure stays failed.
The successor merges actual main1e52d310 and removes the redundant pre-discovery
cookie query: all metadata is read-only/private on the caller's single snapshot,
and final schema validation still precedes atomic acceptance. The publication
cookie check is unchanged. A new changed-schema control refuses the second table,
preserves earlier owned metadata, and proves that stale output cannot publish.
Strict accounting prints its observed tuple after physical guard returns; an exact
CI package/binary/test override makes only that successful output visible; its
positive/128SQL/50000VM/zeroautoindex assertions and measured interval are unchanged.
All26successor names/work outcomes areUNRUN; no general schema-scale work bound,
authority/lifetime proof or activation follows. Current-main hosted/Intake and
curator eligibility remain required; the curator owns the next queue action.


## Fixture-only retained-prefix reclamation controls

The next slice uses the same single-consumer file-backed/WAL Store fixture,
with 32 admitted inputs, 16 applied and 16 pending. The ordinary prune helper now
delegates its identical transaction body to a private helper shared by fault
and crash controls; no production Installer, capture, prepared-page, Runtime,
schema, scheduling or writer/reader finalizer changes. Existing strict/growth
control bodies and their 128 SQL / 50,000 VM / zero-autoindex assertions stay unchanged.

New controls independently copy image/reference/native/output rows and reconcile
actual retained and journal byte sums, row counters and lifetime total. They
witness actual image and journal DELETEs before outer rollback or a refused
COMMIT, remove fixture hooks before returning the writer (including a caught
panic), require an idle writer and check the specific SQLite commit-hook error.
SQLite documents that a nonzero commit hook converts COMMIT to rollback:
https://www.sqlite.org/c3ref/commit_hook.html and
https://www.sqlite.org/rescode.html#constraint_commithook. The manifest, hooks
and oracle remain fixture-owned, not a production fatal-cleanup contract.

Separate exact-name child controls terminate the process before/after prune
COMMIT. A flushed stdout marker and a file+parent-directory-synced phase artifact
identify the phase; the parent requires both marker and distinct exit code
before reopening. A synced pre-prune oracle checks all rows and counters after
reopen, including retained_bytes and total_rows. Open performs no derived
replay or counter repair; remaining pending inputs must still publish correctly.
This does not upgrade the existing sidecar lifetime manifest to production
all-instance restore/replacement authority.

One maintenance scope covers the ordinary background prune acquisition, SQL,
COMMIT and physical writer return, ending before diagnostic/oracle queries. It
prints actual SQL/VM/fullscan/sort/autoindex work and removed rows/image bytes,
and requires positive SQL and VM, <=128 SQL, <=50,000 VM and zero-autoindex. A
same-name no-feature twin fails rather than omits the requirement. The only new
nextest override exposes successful output; no retry/selection/thread/budget
changes. All five new unique names are UNRUN at source authoring, including
both actual crash phases; arithmetic is not a successful work tuple.

These controls do not fix metadata inventory growth: #2173 measured whole-page
128/129/136/160/256 statements at 0/1/8/32/128 added unrelated tables, with
zero headroom at the base. Reusable schema/lifetime authority, multi-consumer
retention, native/canonical mutation closure, physical-schema/work-at-scale,
unsafe cleanup/quarantine, hold/CPU and production opt-in remain open. No new
factory, worker, proof capability, admission policy or source activation.

## Two-consumer production journal controls (fixture only)

The next two controls register distinct fixture operators over the same source
and existing namespace-keyed output/coverage tables. They drive the production
`Installer::prune_journal` directly through existing managed background writer
transactions. The wrapper performs no minimum/cutoff calculation or journal
deletion of its own. It does not invoke the single-consumer image-prune helper:
all 32 immutable images stay retained, within the existing 256-image cap.

One control installs both prefix roots, advances A to 32 and B to 16, and checks
that only the first 16 journal references disappear. A second prune must remove
zero until B consumes the remaining original references. This second call is
necessary to discriminate a fast-root cutoff when each call is capped at 16.
B's current-source status and getter explicitly refuse at revision 16; after B
reaches 32, both namespaces match native-derived ID/subject/kind/digest output.

The other control uses an actual active follower catchup job at revision zero
without a published root. That job must pin every reference even while A is
current. Only an explicit committed cancel releases the job's pin. Cancellation
must leave the other's separately ready root intact. A captured stale follower
page refuses publication without changing its job/root/coverage/output rows or
the source journal counters. The stopped follower remains unavailable; it is
not automatically restarted or repaired. Cancellation of a separately published
follower root is outside this control.

These two names are UNRUN at source authoring. They claim only journal retention
for this fixed two-consumer registry, not image reclamation across two consumers,
crash and reopen with two consumers, multiconsumer rollback/refused-COMMIT, or a
whole-prune accounting tuple/nextest override. Those are deferred and unverified.
Existing single-consumer crash and work receipts do not qualify them. No new
nextest setting, production table/trigger/library/Runtime/factory/worker or
authority/lifetime proof is added, and all earlier controls and ceilings remain
unchanged. Metadata growth remains adverse and the original strict page has
zero statement headroom; any current-main failure must retain its actual tuple.

## Two-consumer journal outer rollback (fixture only)

One appended control calls the production `Installer::prune_journal` inside an
existing managed background writer transaction, with two distinct installed
prefix roots at A=32 and B=16. The library alone selects the cutoff and deletes
journal references; the single-consumer image-prune helper is not called.
An owned within-transaction witness must contain the byte-identical original
references 17..32 and their independently summed row/byte counters, while both
consumers' complete root/job/coverage/output states stay unchanged. A no-op
prune cannot satisfy the changed-rowset and changed-counter assertions.

Separate checked WAL readers must see the original committed state while that
transaction is open. After explicit successful rollback and an idle-writer
assertion, the physical writer loan ends before fresh readers compare all
source/native/image/journal/counter state and both complete consumer states.
The owned changed witness survives rollback. Payload width validation is a
post-copy oracle bound, not production pre-copy eligibility. There are no
sleeps, timing assertions, callbacks or hook replacements.

A predetermined committing arm then removes 16 references; a second prune must
remove zero while B is still at 16. B consumes the original retained references
to reach 32 and matches native-derived ID/subject/kind/digest output with A;
only then may the final 16 references be pruned. All 32 images remain unchanged.
Any transaction/check/rollback error fails the control without a fallback.

The new name is UNRUN at source authoring. This covers successful outer rollback
only after hosted qualification. Two-consumer refused COMMIT, crash/reopen,
image reclamation, whole-prune accounting and unsafe-cleanup containment remain
unverified, alongside source authority/lifetime and production adoption. All
previous controls, helpers, strict/growth/crash bodies, limits and nextest
settings remain unchanged. The original strict page has zero statement
headroom and eight characterized growth pages remain over 128; no old work or
crash receipt qualifies this new two-consumer path.

## Two-consumer journal refused COMMIT (fixture only)

One appended normal control uses the same fixed A=32/B=16 installed prefix
roots, actual production journal prune and copied-reference oracle as the
outer-rollback control. It records an owned changed journal/payload/counter
witness before COMMIT and requires both consumer states unchanged. Separate
checked physical readers see the original committed state while the write
transaction is open. Only the private Plain/no-observer fixture installs a
refusing SQLite commit hook: an external marker records invocation, the hook
returns true, and the error must downcast to SQLITE_CONSTRAINT_COMMITHOOK.
An arbitrary error, a no-op prune, delete-only or decrement-only maintenance,
or unexpected successful COMMIT cannot satisfy those combined assertions.

All fallible work is inside catch_unwind after installing the hook. The hook
is removed before the idle check and any result/panic classification; an
unexpected panic resumes after cleanup. The physical writer loan ends before
fresh reads require full source/native/image/journal/counter conservation and
both complete root/job/coverage/output states. An owned changed witness must
survive the refused commit. This fault is restricted to the fixture's ordinary
writer with no existing hook/observer owner; it adds no production hook API or
quarantine contract. Post-copy payload length checks remain oracle bounds.

A predetermined committing arm then removes 16 references and must remove
zero on the next call while B is still at 16. It also detects a refusing hook
left installed. B consumes the original remaining references and reaches the
native-derived output at 32 before the final 16+0 prune. All 32 images stay
unchanged. The second zero rejects a fast-root-only cutoff, but this fixed
ordering cannot distinguish an implementation that only considers slow B;
complete minimum selection for arbitrary consumer orderings is not claimed.

The new control is UNRUN at source authoring. It is a discriminating fixture
fault, not a production mechanism, storage change or GLOBAL-VIEW qualification.
Crash/reopen with two consumers, image reclamation, whole-prune accounting,
failed-cleanup containment, source authority/lifetime/restore and production
adoption remain deferred. All earlier prefixes, helpers, strict/growth/crash
controls, limits and nextest settings are unchanged. The strict 128-statement
ceiling still has zero headroom and eight growth pages remain over it; no
old work/crash result qualifies this new path.

## Mirrored installed-root journal cutoff (fixture only)

One appended normal control reverses the fixed ordering: primary A is at 16,
while follower B is at 32, over 32 original admissions and distinct installed
namespaces. A's stored prefix-ready bit remains true, but current-source status
and its current getter refuse. A's 16 expected ID/subject/kind/digest rows come
from the original images' revisions joined to native rows, with native bodies
canonicalized independently of produced output. B matches the full native
oracle at 32. Complete physical root/job/coverage/output snapshots are retained.

The actual production journal prune must remove 16 and then zero while A is
still at 16. A B-only, MAX or source-head cutoff can hide behind the first call's
16-row page cap but fails the second call. Together with the opposite ordering
in the existing controls, this rejects either fixed-consumer choice; it is not
a minimum proof for arbitrary consumer orderings, registry sizes or active jobs.
Copied original references and their payload-byte sums define the entire pinned
retention state; the oracle never passes a cutoff to the library. Both complete
consumer states and all 32 original images must remain unchanged by pruning.

Before A consumes the original remaining references, the test derives its full
expected advanced state from the validated pinned state and native output.
Only primary revision/generation, coverage and output may change; physical
root bindings/status and the full job row are checked against their previous
values. Source, journal payloads, image/lifetime counters, images, native rows
and B's complete state stay fixed. Both current getters must then match the
native oracle at 32 before the final 16+0 drain. The old equal-image/reference
helpers are intentionally not used after journal-only pruning and remain
unchanged. No advanced actual snapshot is accepted as its own oracle.

The new name is UNRUN at source authoring. It adds no hook, copied minimum,
production SQL, library/helper change, nextest setting, worker or activation.
Image reclamation across two consumers, crash/reopen, whole-prune work/hold/CPU,
failed-cleanup containment, source authority/lifetime/restore and production
adoption remain deferred. Original strict/growth/crash controls and bounds are
unchanged: the 128-statement strict page has zero headroom and eight growth
pages remain adverse over that ceiling. No previous work or crash receipt
qualifies the mirrored path, and oracle bounds are not pre-copy certification.

## Two-consumer journal post-commit process death (fixture only)

An appended parent/child pair targets only process death AFTER the production
journal prune has committed and after a fresh durable phase marker. It uses
`process::exit(78)` with the Store still live, skipping Rust destructors and
any explicit Store close or checkpoint. The child explicitly flushes its stdout
marker. This is not SIGKILL, power loss, death during COMMIT or an uncommitted
crash. It makes no claim that SQLite performed no automatic checkpoint; the
point is reopening without the fixture's normal Store destruction path.

The parent validates A=16/B=32 using original images and native-derived output,
then owns the full retention snapshot and both complete physical consumer
states. It independently derives the expected pinned and advanced states and
the final empty journal before dropping the original Store. No parent reader,
writer guard or connection survives into the child. A narrow private reopen
adapter uses the unchanged external-manifest identity gate and replaces only
the pure local operator registry. The follower namespace is supplied from the
parent's original value, never discovered from SQL. No schema, start, bootstrap,
readiness, manifest or process-gate bypass is added. This is still the fixture's
limited same-file lifetime check, not production incarnation authority.

The child commits actual production prune16 then0 while A stays16 and B32,
returns its physical writer loans, syncs a fresh per-parent phase file and its
directory, prints and flushes the marker, then exits78. That code was unused in
the pinned fixture. The parent checks the distinct code, exact stdout line and
bounded matching phase file BEFORE opening any Store. The child's ordinary
no-env return cannot establish death; only the parent asserts these boundaries.
No hook, observer, fault callback, sleep, retry loop or timing assertion is used.

After reopening, the full independently expected original17..32 payloads and
row/byte counters, all32 immutable images/native/lifetime fields and BOTH owned
consumer states must match. A remains current-source unavailable; B's current
getter matches native32. Another zero prune must preserve the slow pin. A then
consumes the original retained references, its complete state must equal the
before-derived advanced oracle, both current getters match native32, and the
final16+0 drain leaves the independently derived state intact. Equal-image/
reference helpers are not weakened or used after journal-only pruning.

Both new names are UNRUN at source authoring. The mutation map lists intended
discriminators by source reasoning only: no mutations have been run or proved
to fail. The fixed two-consumer scenario is not an arbitrary-registry minimum
proof. Uncommitted process death with two consumers, image reclamation, whole-
prune work/hold/CPU, source authority/lifetime/restore, failed-cleanup containment
and production adoption remain deferred. All earlier prefixes, helpers, strict/
growth/crash controls and limits are unchanged. The landed strict128-statement
page has zero headroom and eight growth pages remain adverse. Skipped merge-
group perf-cost is not a new cost pass, and old receipts do not qualify this path.

The first hosted post-commit parent reached its exit78 gate but failed the exact
stdout-line assertion on all three attempts; phase-file and reopen assertions
were not reached. The child now explicitly starts its marker on a new line so
libtest's progress prefix cannot share that line. The exact-line assertion stays
unchanged and includes captured stdout/stderr on failure. The failed log did
not retain the child's captured output, so progress-prefix framing is a source-
based explanation, not an observed byte transcript. No gate or limit is relaxed,
and the first failed control does not qualify the reopen or recovery behavior.
