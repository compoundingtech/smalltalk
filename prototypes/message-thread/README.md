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
