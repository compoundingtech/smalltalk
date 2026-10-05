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
replaces one child row inside the writer transaction. Replica-record state
`repaired` removes that claim's assignment; reopening uses persisted rows;
the versioned rebuild reads claims and desired before publishing. The oracle
checks replacement ordering, explicit null, invalid first desired child,
repair and unrepair, deletion predecessor, rollback, rebuild and reopen.

In invented 1,000/10,000 distinct stale-subject cohorts, the two-member
parent seek used 16/15 SQLite VM steps and an unrelated new desired child
write used 156/156. In 1,000/10,000 assignments for one child, deleting the
latest assignment used 167/158 VM steps and reinserting it used 572/572.
These numbers include the prototype's indexes, especially the existing
`claims_batch_index`. They are source-model evidence only; real Store work,
startup/backfill, bytes, durability and p99 remain unmeasured.

Before production integration, use current-main `store.rs` schema near
`message_reply_edges`, its open/backfill path and runtime projection hook;
classify the new table as a rebuildable local cache in canonical audit.
Triggers on `claims`, `desired` and `replica_records` should catch ordinary
append, selected declaration and repair, projection replay, checkpoint,
rollback and retirement without edits to common record/admission/sealer
methods. The production key must match `smallclaims::store::canonical` exactly,
including legacy batch position and multiple replica records per claim; this
model uses one replica record per claim. A versioned migration must build in
bounded chunks or expose explicit readiness rather than scan all history at
every open. Test each mutation against `message_view_tx`, then record indexed
1k/10k read and writer growth on the real schema before replacing the
historical-candidate query in draft #1508.
