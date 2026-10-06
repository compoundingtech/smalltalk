# Selected message reply edge prototype

This isolated SQLite model starts from main `079e12497822112c25931460f16e6e8693596f9c`.
Run `python3 prototypes/message-thread/selected_edge_oracle.py`. It does not
change Store or establish daemon latency.

`reply_assignments` keeps each actual `in_reply_to` assignment in canonical
claim order, with an index that seeks the newest assignment for one child.
An explicit JSON null is retained as an assignment and masks a selected
declaration; an absent actual field leaves the declaration eligible. The
declaration's **first** `in-reply-to` child decides its value, matching
`canonical_child_string`. A selected child has at most one row in
`selected_reply`, indexed by `(parent, child)`. Thread expansion seeks only
these selected children, so historical stale edges do not enter its candidate
prefix. The actual message view still validates every returned child.

The model uses SQLite triggers on claim insert/delete, selected desired
insert/update/delete, and replica-record insert/update/delete. Each refresh
replaces one child row inside the writer transaction. A `repaired` replica
record still contributes its retained claim to `message_view_tx`, and its
position remains part of canonical order. Reopening uses persisted rows;
the versioned rebuild reads claims and desired before publishing. The oracle
checks replacement ordering, explicit null, invalid first desired child,
repair and unrepair, deletion predecessor, rollback, rebuild and reopen.

In invented 1,000/10,000 distinct stale-subject cohorts, the two-member
parent seek used 16/15 SQLite VM steps and an unrelated new desired child
write used 156/156. In 1,000/10,000 assignments for one child, deleting the
latest assignment used 167/158 VM steps and reinserting it used 249/249.
These numbers include the prototype's indexes, especially the existing
`claims_batch_index`. They are source-model evidence only; real Store work,
startup/backfill, bytes, durability and p99 remain unmeasured.

The first prototype recomputed a legacy claim's position by counting earlier
claims in its batch on every insert. A fixed 1k→10k batch made that write scan
grow to 3,301→30,301 VM steps in independent review. The revised model stores
`store_index` as the position key for **legacy-only** batches. Among those
claims it has exactly the same order as the canonical COUNT rank, including
after deletion. For **fully recorded** batches it uses MIN(replica-record
position); the oracle now exercises two records for one claim and repair
state. In a single legacy batch with 1k→10k assignments, sampled insert and
delete work is 249→249 and 158→158 VM steps. A small independent canonical
COUNT fold checks the selected answer after order, repair and deletion
changes. A deliberately mixed recorded/legacy batch gives the wrong selected
parent under this shortcut; the model detects it and rolls the write back.

This is conditional source evidence: current production code has no enforced
batch mode invariant. Prove complete recorded batches across local append,
replica admission, repair, checkpoint and reopen, and validate existing stores
before using the order-equivalent key. If a mixed batch is possible, use an
indexed exact rank source or keep the projection unready until it is repaired;
never silently publish this model's key for that batch.

`mixed_rank_oracle.py` is the follow-up exact-rank model. It puts recorded and
legacy assignments in separate indexed head lanes for each child. The recorded
head uses the minimum replica-record position; the legacy head uses the newest
store index. A 63-level per-batch count trie computes the legacy head's current
COUNT rank using indexed point seeks, so the two heads can be compared by the
same canonical tuple. The independent slow COUNT fold stays outside writer
measurements. Its fixture first proves the preceding `store_index` shortcut
returns the wrong parent, then checks the two-lane answer under record identity
and position changes across two batches, repaired state, a claim store-index
move, insertion of an earlier unrelated claim, and deletion. `--growth` builds
1,000/10,000 children in four cases: every parent changes; only two parents
change while an unrelated batch grows; a recorded-only changed batch grows
while no parent changes; and a mixed changed batch grows while no parent
changes. It records enumerated and changed keys, VM steps, statement
count, SQL trace text bytes, rank-node writes and seeks, row changes, and
allocated page-byte delta. SQL text bytes and page allocation are proxies,
not disk-write bytes. Full-fold spot checks run after each measured mutation.
`--small-growth` exercises the same cases at 10/100.

The first enumerator revisited **all** message children in a changed batch.
Its frozen `b56368c3` receipt showed 1,000→10,000 unchanged recorded-only
parents costing 180,662→1,773,662 VM steps. The later model seeks only legacy
assignments after the changed store index, plus directly mutated subjects,
and skips selected-row writes when the answer is unchanged. That removes
recorded-only children from rank-shift work, but mixed children whose parents
stay unchanged still require evaluation. An unrelated predecessor deletion can
also actually flip every mixed child's selected parent; this is inherent
affected-key fanout. Both the high original constants and the remaining
mixed no-change scan are failing writer-cost rows, not production acceptance.
The rank trie touches 63 count nodes per insert/delete and needs real-schema
transaction, durability, replay and byte-cost proof. No shared Store or API
method uses this model.

`rank_gap_tree_oracle.py` is an isolated follow-up to the mixed no-change
case. It keeps one legacy-head leaf per child only when its canonical prefix
ties a recorded-lane head in the same batch. The leaf stores the dynamic
legacy COUNT rank minus recorded position. A sparse range-add tree applies a
predecessor insertion/deletion to later leaves and prunes subtrees whose gaps
are too far from a possible winner crossing. Invented 10/100-child fixtures
visit 19/22 nodes with zero candidates when the recorded position is far
ahead, versus 43/312 nodes and 10/100 changed parents when every comparison
crosses; 500 seeded set/delete/shift operations match a full dictionary
oracle. These are Python node visits, not SQLite VM steps. The tree is not
connected to `mixed_rank_oracle.py`, does not handle cross-batch prefix changes
or persistent rollback/reopen, and does not cure the preceding measured mixed
writer-cost failure. Any real implementation must update leaves transactionally
when either lane head or its canonical prefix changes and prove bounded
storage/replay/retirement.

`coupled_range_oracle.py` connects that tree to the existing SQLite model for
one invented predecessor deletion and one direct replica-record position
change. At 10/100 mixed children, the full canonical fold and selected rows
agree after refreshing zero far-recorded children or all 10/100 crossing
children. This is a small semantic check only: its tree is in Python memory,
its setup is unbounded, and it does not cover canonical prefix changes,
multiple lane heads, rollback, reopen, checkpoint or actual Store hooks. The
SQL model's earlier measured mixed-batch writer row remains failed.

`persistent_gap_tree.py` is a source-only SQLite persistence sketch for this
range summary. It stores per-child lane-head identity and parent fields, plus
per-batch sparse count/min/max/lazy nodes. The gap uses decimal TEXT to avoid
silently narrowing the canonical u64 replica-record position; positive claim
store indexes remain within SQLite's signed INTEGER domain. `replace_head`
removes an OLD leaf and inserts a NEW leaf, including cross-batch moves;
`shift_after` applies one predecessor rank change and returns only possible
crossings. Both operations must share the claim, record, rank and selected-edge
writer transaction. `persistent_gap_tree_fixture.py` is a queued invented
rollback/reopen/move/removal check, not yet executed. The code is not wired to
Store or the SQLite rank model, has no migration, GC or startup/backfill plan,
and has no measured node/statement/byte or production lifecycle result. It
cannot clear the frozen b563 or later a8 writer-cost failures.
`refresh_child_leaf` now derives the two indexed lane heads, checks equality
of all five canonical prefix fields, and keeps a leaf only when their parents
differ. It avoids rewriting an identical persisted leaf. The source-only
`persistent_pair_fixture.py` couples that derivation to the SQLite claim and
record model, with a queued rollback/reopen assertion. The source-only
`persistent_pair_growth.py` measures a future writer transaction including
rank-index changes, persisted tree SQL, selected-parent writes, SQL VM steps,
statements, row changes and page allocation for far unchanged, all changed,
and fixed two-child output under unrelated 1k/10k history growth. Neither
fixture has executed, so none of those correctness or cost rows are green.

Before production integration, use current-main `store.rs` schema near
`message_reply_edges`, its open/backfill path and runtime projection hook;
classify the new table as a rebuildable local cache in canonical audit.
Triggers on `claims`, `desired` and `replica_records` should catch ordinary
append, selected declaration and repair, projection replay, checkpoint,
rollback and retirement without edits to common record/admission/sealer
methods. The production key must match `smallclaims::store::canonical` exactly,
including legacy batch position and multiple replica records per claim; this
model now uses multiple replica records per claim but simplifies their full
envelope lifecycle. A versioned migration must build in
bounded chunks or expose explicit readiness rather than scan all history at
every open. Test each mutation against `message_view_tx`, then record indexed
1k/10k read and writer growth on the real schema before replacing the
historical-candidate query in draft #1508.
