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
