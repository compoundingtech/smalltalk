# Receive replay source audit and unrun controls

Review input for [the proposed receive replay design](receive-replay-design.md) and
[#2861](https://github.com/compoundingtech/smalltalk/issues/2861). This audit is pinned to
`24d78d71b9b99a2614de215c1f1660e1b2fa7d2e`. It changes no runtime path.
The table-name swap and randomized interleaving controls below are **UNRUN**. Neither a
production row/input witness nor a complete generation routing certificate is supplied.

## Counted source inventory

The reproducible literal census and all path/line entries are retained in
`doc/fleet/smalltalk/graph-projection-divergence/2026-10-14/receive-replay-source-inventory@bf7c00b4ec38efa9d34748540b59a106cb295a74deee3ecd93d1ce6ba6ea6f6d`.
It includes the scanner and a hash-bound compressed JSON inventory. Decompression is data
inspection, not an instruction to execute a control.

Scope: all 417 tracked Rust files in `crates/st3/src` and `crates/smallclaims/src`, including
`cfg(test)` code and fixtures. Rust comments and character literals are skipped; SQL comments
are removed before matching SQL-leading string literals. The census finds 3,530 literal sites,
of which 300 contain templates. Counts below are **literal sites**, not SQL statements,
production tables, installed objects, or expanded execution. Roles overlap. Identifiers can
include CTEs, aliases, test tables and placeholders. Macro expansion, conditionally compiled
reachability and dynamically assembled identifiers still need a reviewed production inventory.

| Pattern | Literal sites | Distinct lexical identifiers |
| --- | ---: | ---: |
| Read (`FROM`/`JOIN`) | 2,307 | 289 |
| Write (`INSERT`/`UPDATE`/`DELETE`) | 1,561 | 243 |
| `CREATE TABLE` | 146 | 231 |
| `CREATE TRIGGER` | 73 | 192 |
| `REFERENCES` within a table DDL literal | 11 | 11 |
| `CREATE INDEX` | 42 | 179 |
| `INDEXED BY` | 148 | 57 |

The verified entry-path walk is narrower than the census: eight graph tables are cleared,
24 logical tables are registered for shared projection digests, and seven post-projection
calls execute in order. None of those inventories equals the complete stage closure.

| Boundary at the pinned source | Routing and hold obligation |
| --- | --- |
| `smallclaims/src/store.rs::project_replication_backlog_chunks` | Native admission, foreground IMMEDIATE transaction, incremental savepoint, replay, hooks, frontier/health and commit; all expanded work belongs to the hold |
| `st3/src/store.rs::REPLAYED_GRAPH_TABLES` (30260) | `desired`, `mission_definitions`, `mission_run_deadlines`, `mission_run_after`, `step_runs`, `revision_proposals`, `run_generations`, `mission_runs`; route each read/write and its dependencies |
| `replay_graph_from_nothing_with_progress_tx` (30279) | Operations, base claims, mission runs, planning, resources, glass heads, directive notes, custom, arrangements, publication reset; eight clear targets omit several rebuilt families |
| `st3/src/store/runtime.rs::after_projection` (406) | In order: custom, resources, glass heads, directive notes, agent messages, limits, local work-lease renewals; seven calls, with queues/cursors and dependencies retained |
| `st3/src/store.rs::PROJECTION_DIGEST_TABLES` (59194) | 24 tables listed below; native blobs/documents/operations are also inputs, so do not duplicate or replace them merely because they are digested |
| `smallclaims/src/store/projection_digest.rs::{register,initialize,columns}` | Four digest state/cache tables; generated per-table triggers, scalar functions, sorted logical columns and initialization scans; route logical identity separately from physical generation names |
| `st3/src/store/publication_facts.rs::{create_schema,reset,flush,install_cache_observer}` | Six local publication tables, source triggers, bounded finalizer, committed cache epoch and native observer; a stage must not redirect them away from native/local evidence |
| `st3/src/store/runtime.rs::finish_schema` | Core/local schema helpers plus optional registered IVM views; every schema hook and dynamically registered view needs routing classification before conversion |
| `smallclaims/src/store/checkpoint_staging.rs` and checkpoint rules | Retained epoch/cut/cursor and proof copies are precedents, not a certificate for all replay inputs; keep trim, repairs and protected source coverage |
| `smallclaims/src/sqlite.rs::{observe_commits,WriterGuard::drop}` | Native finalizers/committed-authority observers complete before acknowledgment/connection return; include their work and failure in the acquired-to-returned hold |

The 24 digest table names are `operations`, `blobs`, `documents`, `desired`, `arrangements`,
`arrangement_registers`, `message_index`, `resource_observations`, `glass_heads`,
`person_directive_notes`, `custom_registrations`, `custom_sources`, `custom_dependencies`,
`mission_revisions`, `mission_definitions`, `mission_runs`, `mission_run_deadlines`,
`mission_run_after`, `run_generations`, `step_runs`, `revision_proposals`, `planning_sessions`,
`planning_candidates`, and `planning_previews`. Existing excluded columns remain part of the
semantics: notably effective local step lease/update dates are excluded from the shared digest,
but remain typed local rows and must survive staging. Cached digest equality cannot prove that.

An implementation audit must classify every census site as native source, staged output,
local overlay, disposable temporary relation, or unsupported dynamic route. It must close
cross-generation joins, raw readers, writable views, triggers, foreign keys, index names,
provider/IVM schema hooks, transaction maintenance and cache invalidation. The 300 template
sites and test-inclusive counts are explicit remaining work; they are not silently approved
production routing. No complete input/row/cache oracle follows from this source audit.

## Routing commitment for the decision

The separate exact classification and source/document set coverage are retained in
`doc/fleet/smalltalk/graph-projection-divergence/2026-10-14/receive-replay-routing-decision-classification@9baa23fa799c8a1ae23d39a07504d31a605093b9ba161aa41aab961df51974cc`.

This classification covers each of the 24 registered digest relations, marks the eight cleared
relations, and classifies all seven hooks. It is a proposed routing commitment grounded in the
pinned consumers below, not a compiled routing certificate. Native source means retained native
content/admission materialization: it does **not** make a derived row independent authority.
Staged output means generation-bound rebuildable rows, with current native mutations mirrored
into the active generation and complete catch-up journal. Local overlay means host-local state
or bookkeeping preserved outside a blanket shared-table swap and applied only under its fresh
identity. Disposable means private preparation, never a published root.

| Digest relation | Base clear? | Proposed class | Evidence and routing consequence |
| --- | --- | --- | --- |
| `operations` | No | Native source/admission materialization | `smallclaims/store.rs::{register_operation_tx,expected_operation,rebuild_operations_tx}`; preserve native idempotency/conflict receipts and checkpoint meaning. Shadow expected rows may be checked privately; reconcile bounded keyed changes under the native writer, never replace a live admission ledger wholesale |
| `blobs` | No | Native source | `smallclaims/store.rs` blob admission and document selection; retain hash-bound immutable bytes and retention leases, with no second independent shadow blob catalog |
| `documents` | No | Native source/binding materialization | `smallclaims/store.rs::select_replicated_document` (8564) and `custom.rs::refresh_registrations`; preserve admitted content/bindings, journal canonical binding-key changes, and certify the native binding cut used by a staged custom fold |
| `desired` | Yes | Staged output | Base replay and `select_replicated_desired`; bind selected declarations and owned-set composition to one generation; effect guards still execute freshly at the current native cut |
| `arrangements` | No | Staged output | `arrangements.rs::{rebuild,project}`; rebuild canonical heads from retained edited claims and route head/register joins to the same generation |
| `arrangement_registers` | No | Staged output | Same reducer; retain winner keys and raw register/tombstone semantics with its corresponding arrangement head |
| `message_index` | No | Staged output | Source schema insert/delete triggers on `claims` (store.rs 837–855) and `backfill_message_index`; route trigger writes to the active generation and journal claim deletion/rebuild, not only new sends |
| `resource_observations` | No | Staged output | `resources.rs::{rebuild,flush,refresh}`; route repaired exclusions and pending-subject flush together; no current list authority from an old root |
| `glass_heads` | No | Staged output | `glass_heads.rs::{rebuild,flush}`; route canonical head/deletion updates and dirty/pending queues without mixing generations |
| `person_directive_notes` | No | Staged output | `directive_notes.rs::{rebuild,flush}`; preserve canonical selected note and repaired-claim exclusions; read-time expiration is not a new replicated clock input |
| `custom_registrations` | No | Staged output | `custom.rs::refresh_registrations`; pin the native document/blob manifest and registry identity before folding custom sources |
| `custom_sources` | No | Staged output | `custom.rs::refresh_source`; bind registration, dependency and canonical source revision in the same root; unsupported provider closure refuses sealing |
| `custom_dependencies` | No | Staged output | `custom.rs::{flush,refresh_source}`; retain dependency edges/cycle evaluation with their source rows; route dirty-edge invalidation |
| `mission_revisions` | No | Native source/content materialization | `select_replicated_mission` (31513) and native adoption append paths; preserve validated authored bodies/claim provenance used by run reconstruction; canonical claim-id corrections are journaled bounded native changes |
| `mission_definitions` | Yes | Staged output | `select_replicated_mission` and owned-set selection; route the selected revision, not a stale catalog entry, under existing owner/definition fences |
| `mission_runs` | Yes | Staged output | `project_replicated_mission_runs` (30608); route state/phase/generation selection and all descendant joins as one root; native request/result custody is retained |
| `mission_run_deadlines` | Yes | Staged output | Run creation/state fold; bind deadline relations to the corresponding run, with exact immutable date semantics |
| `mission_run_after` | Yes | Staged output | Run dependency projection; route predecessor/dependency membership with the same run generation, preserving terminal/stop fences |
| `run_generations` | Yes | Staged output | Three-pass run replay; route generation lineage/supersession with run/step state; an old generation never grants successor effect authority |
| `step_runs` | Yes | Staged output with local overlay columns | Run/work/carried folds plus `reapply_local_work_lease_renewals_tx`; staged durable state and native timing claims stay bound to the attempt. Apply quiet lease/update dates only for the same owner/incarnation/attempt; those digest-excluded columns are not discarded |
| `revision_proposals` | Yes | Staged output | Run replay proposal lifecycle; route applied/pending/draining status with the run and current generation; do not duplicate or retime native acceptance |
| `planning_sessions` | No | Staged output | `rebuild_planning_tx` (22008); generation-bind session and its mission/run targets; preserve quarantined-claim handling |
| `planning_candidates` | No | Staged output | Same canonical planning fold; route candidate/session associations and native document references coherently |
| `planning_previews` | No | Staged output | Same fold; route preview/candidate/session and target-generation joins to the root; cached preview is not an effect authorization |

Primary relation counts: four native materializations and twenty staged outputs; the eight base
clear targets are all within the twenty. `step_runs` additionally has local overlay columns.
None of these 24 relations is disposable. This does not claim that every native table is listed:
claims/batches/replica/checkpoint/principal/idempotency/local-runtime relations remain native,
and their exact source coverage must be closed independently. Native materializations can change
while an old staged root exists: joins/digests must certify a matching cut or refuse/recapture,
never label current native rows with an old staged certificate. That integration is unproved.

| Post-projection hook, in actual call order | Proposed routing class | Required consequence |
| --- | --- | --- |
| `custom::flush` | Staged output | Four custom relations/dirty queue participate in the stage; pin native manifest/dependency inputs and provider identity |
| `resources::flush` | Staged output | Resource observations and pending-subject state bind to the target generation and repaired input cut |
| `glass_heads::flush` | Staged output | Head rows and pending/dirty queues bind to the stage; do not leave a native trigger writing an unselected shadow |
| `directive_notes::flush` | Staged output | Note rows and pending/dirty queues bind to the canonical stage input |
| `agent_messages::flush` | Local overlay/bookkeeping | Counts/sends and coordination metadata are native host bookkeeping outside the 24-table shared digest root. Reconstruct the pure count image privately if required; run no coordination/effect from scratch and preserve current native receipts/queues. Separating `flush_with_coordination` needs exact owner review |
| `limits::flush_limits` | Local overlay/bookkeeping | Preserve host account readings/seats and `meta` cursor/version; any stage dependency gets a fixed local snapshot/cut, not a shared-generation table replacement or replayed stop effect |
| `reapply_local_work_lease_renewals_tx` | Local overlay | Retain the native renewal table and apply only to the current matching staged step attempt/owner/incarnation; no fabricated renewal claim or reset clock/budget |

Hook counts: four staged-output hooks and three local-overlay/bookkeeping hooks. The
`heal_replay_base_order` temporary relation (store.rs 30357) is **disposable**: create/fill/drop it
in private preparation, never retain its live-writer lifetime or expose it as a source root.
Native publication finalizers, digest triggers and committed authority observers are additional
native boundaries, not an eighth post-projection hook to bypass. This classification names the
commitment; dynamic provider closure, native/staged cut composition, hook side-effect separation,
raw query routing, trigger/FK/index conversions and mixed-build enforcement remain unqualified.

## Non-claim inputs and journal closure

These source consumers demonstrate why replaying the admitted claim bodies alone is incomplete.
The journal must represent authoritative source transitions; derived dirty queues/cache rows
are either reproducibly rebuilt or snapshot-bound, never treated as independent authority.

| Actual source or local input | Consumer and transition to cover |
| --- | --- |
| Batch origins/sequences, retained replica metadata, repair state and canonical positions | Base replay uses `claims JOIN batches`, repaired exclusions and `canonical_sql`; metadata repair/rank changes invalidate ordering even without a new claim body |
| Checkpoint claim/operation metadata and persistent repaired exclusions | `smallclaims/src/store.rs::{expected_operation,expected_operations,rebuild_operations_tx}` reads checkpoint records and `projection_digest_repaired_claims`; trimming a receipt must not erase its meaning |
| Content-addressed blob bytes, document bindings and mission revision content | `custom.rs::refresh_registrations` (223) reads `documents JOIN blobs` and validates the manifest hash; base mission/document selection also resolves retained content; bind content and retention, not only a document name |
| Quiet member-local work lease renewals | `st3/src/store.rs::reapply_local_work_lease_renewals_tx` (29017) reads `local_work_lease_renewals` and overlays only the matching attempt/owner/incarnation; it is not reproduced by replicated claim bodies alone |
| Local startup observations and publication bootstrap state | `publication_facts.rs` source triggers consume `local_observations`; native finalization and committed cache epochs must remain local and correctly fenced across replacement |
| Dirty/pending queues and cursor/version metadata | `custom`, `resources`, `glass_heads`, `directive_notes`, `agent_messages` and `limits` flush routines consume local queues or `meta` cursors; recompute from the fixed source or bind and journal their completion, without losing pending work |
| Native authority/admission/checkpoint and registered reducer identities | Runtime registry, executable/schema/storage/rules identities and protected retention are part of the certificate; a schema-hook or manifest change refuses resume rather than reinterpreting old scratch output |
| Preserved operational state outside replay outputs | Idempotency receipts, physical start results, local stops/attention and admission frontiers must not be erased by projection publication; classify any projected consumer before deciding whether it needs a journal entry |

This is a demonstrated input inventory, not proof that these eight categories close every
registered/custom provider. Nested validators and dependency edges in `custom.rs::refresh_source`
(499), coordination effects in `agent_messages.rs::flush_with_coordination` (156), and optional
IVM schema hooks require their own exact closure review. A caller clock or configured provider
must not enter a replicated row without a deterministic input contract. The tiny fixture oracle
must distinguish local overlays from shared digest columns and report the first differing
column, including whether it is a timestamp.

The full authored interleaving design is retained in
`doc/fleet/smalltalk/graph-projection-divergence/2026-10-14/receive-replay-interleaving-v2-control-design@c6c5946110d0cc006df6f8532a9b4b3f8f39b26c09e8f529270af2ff35bfd212`.
It uses disposable origin/replica/stage/reference stores,
seeds 0, 1, 17 and 2861, and at most 64 dependency-valid events per seed. Its finite input cap is
256 claims, 256 KiB payload and 4,096 reference rows. Events cover admission, duplicate/empty
receive, repairs, checkpoint/trim, late canonical order, document/blob retention, local renewals,
queue/finalizer completion and crashes at capture/build/copy/seal/commit/ack boundaries.
Seed2861 additionally delivers the same retained input to a replica in two dependency-valid
orders; compare only after their certified cuts match. Restart the disposable stage store
mid-copy after a real page/cursor commit, with another bounded source event in the gap; resume
only a matching manifest/owner/cut or refuse and clean.
Actual native/page acknowledgments select scheduling points. Record the generated schedule
before applying it; authority refusal is a result, not permission to manufacture state.

At the finite target, compare only after origin, staged and independent replay inputs certify
**the same cut**, including repairs and local overlays. Compare typed rows, native receipts,
operation identities and independent fixture-only row hashes, then compare cached digests to
that oracle. Deliberately omit a local renewal, drop a repair invalidation and change the source
after sealing: each must cause the relevant row mismatch or CAS refusal. Also exercise wake in
Pending-to-wait and crash after physical commit before acknowledgment. Missing adapters prevent
execution; this specification is UNRUN and supplies no parent-fails/fixed-passes claim.
F3 results gate implementation landing, not the Nathan proposal question.

## Disposable table-name swap spike

F1 remains unproved. The exact separable source and recipe are retained in
`doc/fleet/smalltalk/graph-projection-divergence/2026-10-14/table-name-swap-v2-growth-source-recipe@4b848ba40d2250083558114bf3694533e4b8274e29d58bd59d168ff24d21fb9e`.
Source SHA256: `9c6536f94a56aee34444cc3344a382ea1adc013950ad13bc4917dae30e107574`.
Only Python `ast.parse` was used; the module was not imported or invoked.

The proposed future unit runs serially on a newly created disposable directory and accepts no
existing database path. Row-scale cases use 0, 1,024, 16,384 and 131,072 rows per side
with 128-byte values in one pair. Schema-scale cases use 1, 8 and 24 live/shadow pairs with the
same 131,072 total rows per side. Thus the 24-pair case has 5,461/5,462 rows per pair; it measures
schema growth at fixed row mass, not millions of rows in every table. The largest single-table
case does not establish a production extrapolation for 24 large tables.

Each pair has two indexes, two insert triggers, a view and a foreign-key dependent table, plus
one common observation table. The 24-pair fixture therefore has 73 tables, 48 indexes/triggers,
24 views and 24 FK edges. These are schema-shaped fixtures, not the actual product schema.
The name-only transaction renames all pairs and includes COMMIT in its measured phase. A second
physical connection warms its cached statement, opens a read transaction and retains a partly
consumed cursor across both the swap and subsequent index repair. It records coherent old/new
results, errors and schema definitions; Python may transparently reprepare, so no raw
`SQLITE_SCHEMA` count or rusqlite cache behavior is inferred.

Global-index-name collision, `INDEXED BY`, view/FK/trigger rewrites, populated index repair and
populated retirement remain separate observations. Index repair is timed while the old reader
is open, including its lock/error and commit behavior. No aggregate swap number hides it.
WAL/FULL durability is retained; SQLite version, PRAGMAs (including legacy ALTER behavior),
schema text and estimated VM progress accompany each phase. Main DB, WAL and SHM logical/
allocated sizes are sampled before and after setup, swap, index repair and retirement, with
separate deltas. Cleanup samples before close, after physical close and after owned removal
and records its own duration; these observations remain unrun and are not hard quota proof. Completed cases emit
raw records incrementally; a genuine error emits its failing phase and stops without retry.

The source's 256 MiB phase-boundary file-size check is **sampled**, not a hard aggregate disk
limit; SQL may allocate before the next sample. The future hosted unit must attest a separate
256 MiB hard owned volume/quota covering DB/WAL/SHM before invocation, or refuse to start.
A per-file `RLIMIT_FSIZE` alone does not meet that aggregate requirement. MemoryHigh 256 MiB,
MemoryMax 512 MiB and the unchanged 90-second queue-inclusive deadline remain proposed,
unadmitted ceilings. Queue/setup/child/rollback/physical close and delegated cleanup all count;
no extra grace, higher cap or automatic retry follows a failure.

The concrete ordinary hosted runner recipe is retained at
`doc/fleet/smalltalk/graph-projection-divergence/2026-10-14/hosted-f1-v2-growth-runner-recipe@2fcef748dfb68a30046337182694ac07a1b39541d3b056e2c3d6eef58f78cceb`.
It proposes one serial supporting Linux job under the existing ordinary PR queue policy, with
this seat as caller and the exact script hash above. Actual execution owner, slot/unit/lease,
Python/SQLite binary pins, hard quota provider and absolute deadline are unset review inputs
for Root/Factory; no local/production invocation, workflow dispatch or new worker is authorized.
After real connection closure, remove only the owned disposable files/volume and retain terminal
cleanup evidence even on cap/error. No Nix/Cargo/native experiment is part of this recipe.

Success requires unchanged logical reader/writer/FK/trigger/index semantics and a bounded final
native hold without deferring populated cleanup into another unbounded hold. A fast two-rename
transaction alone is insufficient. Python statement caching does not certify rusqlite's cache,
and this mechanism spike cannot prove the actual WriterGuard/admission/observer boundary.
Those interfaces need a separate native control after a viable alternative is reviewed. If a
fully qualified table-name swap meets the boundary, reconsider the generation/format proposal.
No impossibility or positive result is claimed from the unrun source.

## Conditions carried to the decision

| Finding | Proposal revision and remaining qualification |
| --- | --- |
| F1 | Name-swap alternative and bounded source recipe are explicit; mechanism/native results absent; no format-free impossibility claim |
| F2 | All 8 clear/24 digest/7 hooks classified above with routing consequences; compiled routing/dynamic-provider closure remains required |
| F3 | Non-claim consumers and mixed-order/mid-copy-restart discriminator specified; native results gate implementation, not the Nathan question |
| F4 | Existing 19→20 format-hold owner join proposed in the design; legacy-19 refusal is insufficient to fence all flat-only version-20 binaries |
| F5 | Proposed error/receive wire shapes and old-client behavior specified in the design; no emitted contract or generated client change |
| F6 | Land #2859 telemetry before implementation; per-guard cause/hold evidence stays distinct from a successful bound |
| F7 | Scratch, active/hidden/retired projections, WAL, retained inputs and ZFS-held blocks share an explicit disk budget with #2849 sorter spill; no quota approved |
| F8 | #2644 is independent and unlanded; retain native observer/permit/ack invariants without requiring its landing as the replay repair |

Docs may be reviewed as a proposal with these controls dormant. Nathan routing stays held until
Intake's required decision evidence exists. No native qualification, incident cause, graph
convergence, persisted-state repair, version reservation or implementation follows from the
source counts, recipe syntax or docs publication.

The previously accepted metadata diagnostic remains separately proposed: output must say
"not a row, cache or input oracle". Any 32-key point-field comparison uses the same certified cut
and key interval on both nodes and refuses a differing/unprepared cut or interval. It reports
whether the first differing field is a timestamp. No live capture or diagnostic implementation
is introduced by this audit.

## Hosted binding source candidate

See [the unrun controller/job binding](receive-replay-hosted-binding.md). It pins the
reviewed growth successor and keeps the queue-terminal/provider cleanup boundary
explicitly unqualified. Ready and invocation require Root exact-source review.
