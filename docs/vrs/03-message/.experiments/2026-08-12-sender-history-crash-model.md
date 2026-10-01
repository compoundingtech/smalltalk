# Sender history crash model

## Question

Can a sender-owned durable intent bridge recipient-first and sender-row publication without false
Sent, silent recipient-only completion, or duplicate keyed retries, without claiming
cross-directory atomicity?

## Method

### Alternatives

| Design | Recipient deletion | Crash between writes | Identical intentional sends | Result |
| --- | --- | --- | --- | --- |
| Scan all recipient inboxes and archives | Loses history | No sender recovery state | Distinct | Rejected |
| Publish sender row first | Preserves history | Can report false Sent | Distinct | Rejected |
| Write recipient then sender without intent | Preserves completed history | Can leave silent recipient-only delivery and duplicate a retry | Distinct | Rejected |
| Deduplicate by payload hash | Preserves history | Can reuse a write | Collapses legitimate identical sends | Rejected |
| Full mutable manifest | Preserves history | Rewrites O(history) state per send | Distinct | Rejected |
| Mutable append log | Preserves history | Torn tail needs a second recovery protocol | Distinct | Rejected |
| Count or per-row marker without a linked head | Cannot detect loss or substitution | Incomplete trust root | Distinct | Rejected |
| New database | Preserves history | Adds an unnecessary storage subsystem | Distinct | Rejected |
| Linked immutable commit ledger | Preserves history | Explicit partial state and resumable filename | Distinct unless the caller supplies the same key | Selected |

Typed `request` publication supplied the reservation precedent: persist the chosen filename and
rendered bytes before idempotent recipient materialization. Its service-principal records remain
outside ordinary Sent history.

### Prototype

A pure throwaway state model used one constant head, one active pointer, immutable rows, and immutable
content-addressed commit nodes. It enumerated eight ordered steps after coverage initialization:

1. create pending,
2. publish active,
3. materialize the recipient,
4. create the sender row,
5. create the linked commit node,
6. atomically advance the head,
7. clear pending,
8. clear active.

The model injected a crash before and after every step. Recovery then had to satisfy every oracle:

- every sender row's filename exists in the recipient set,
- coverage is never `since` while intent is pending,
- exact traversal reaches genesis in the head's declared count,
- every node digest, predecessor, ordinal, and row digest verifies,
- reachable node and row sets exactly match their directories except state explained by active,
- orphan pending is partial and recoverable,
- head-advanced stale active or pending state cleans up without republishing.

Thirteen falsifiers removed or substituted the head, node, or row; added unexplained state; changed
digest, predecessor, ordinal, count, genesis, or version; and required rejection. The head is the
bounded trust root. Coordinated adversarial rewrite of that head plus all matching sender state is
outside the local loss and corruption contract.

### Performance

Run the exact candidate binary against one captured real catalog with at least 557 discovered agents
and at least one sender-owned row. Verify both counts first. For `message sent <sender> --json` and
the same catalog's `message ls <sender> --json`, run one untimed warm-up followed by ten independent
timed invocations. Record each wall-clock duration in milliseconds, sort each ten-sample series, and
take nearest-rank p95 (sample 10). Record `sent p95 / message-ls p95`. Sent p95 must be no greater
than 1.0 second and no greater than twice message-ls p95.

The benchmark must leave an unrelated recipient box structurally unreadable while Sent still returns
the sender rows. It reports the exact Git commit, catalog agent count, sender row count, both exact
commands, both ten-duration series, both p95 values, and the ratio. Synthetic or empty sender data is
not a substitute for this axis.

## Result

### Prototype and executable controls

The prototype returned
`{"result":"pass","crashCases":9,"corruptionCases":13,"writeComplexity":"O(1) history","readComplexity":"O(history)"}`.

The first ledger run exposed one boundary the simpler active/pending rule missed: after the head
advances, active may remain while pending cleanup has already completed. The head proves that active
pointer is stale recoverable state. Before head advancement, active without its matching pending
record fails closed.

The model's invariants are executable in
`tests/message_cli.rs::keyed_retry_recovers_every_crash_boundary_without_false_sent_or_duplicates`.
The shared coverage and row contract is executable in
`crates/st-wire/src/message.rs::sent_rows_carry_to_and_coverage_never_collapses_unavailable_into_empty`.
The structural RED control makes an unrelated recipient box unreadable before Sent enumeration; a
recipient-scanning implementation therefore fails while a sender-owned implementation succeeds.
The throwaway driver is not part of the repository.

### Benchmark

Pending exact-candidate Sent and same-sender message-ls measurement. No performance result is claimed
until both p95 values and their ratio are recorded under the method above.

## Conclusion

The linked ledger satisfies the bounded crash/retry and corruption oracles. Recipient-first ordering
prevents false Sent. The constant head makes every completed prefix verifiable without O(history)
write work. Durable pending and active state make intermediate writes explicit and resumable.
Unkeyed post-commit response ambiguity remains the declared at-least-once tradeoff.

## VRS Impact

The selected invariants define `MESSAGE-R03` through `MESSAGE-R08` and the transaction, coverage, and
retry sections of [`../spec.md`](../spec.md). The pending performance result gates `MESSAGE-R10`.
