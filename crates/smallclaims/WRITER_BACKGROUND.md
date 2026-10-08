# Foreground and background writer loans

`WriterConnection::write()` and `batched()` remain foreground, ordered within the original
admission channel. `write_background()` borrows the same connection and returns the same
`WriterGuard`; its transactions run the same prepare/finalize callbacks, rollback, committed
index and returned-guard observers. A caller must drop the guard before acknowledging durable
work. No claim, source certificate, ready token or authority decision follows from scheduling.

Use a background loan for one bounded maintenance or prepared publication page. Release it
between pages and recheck source identity, authority and the full prepared CAS readset after
acquisition. Operators and expensive reads/reduction stay off the writer. SEND, harness/work,
renewal and externally requested durable producer updates remain foreground. This change
classifies no existing production caller automatically.

The one writer scheduler prefers queued foreground work while a background loan is waiting.
After eight foreground dispatch turns it serves one pending background loan, then repeats.
A turn is one foreground loan or one existing bounded group-commit batch (up to 256 writes,
with its existing cooperative 50ms batch window). Each class is FIFO. A background-only queue
progresses immediately. The scheduler pops class heads; it does not enumerate backlog or scan
SQL history. An empty-to-nonempty notification wakes the existing admission channel.

This bounds overtaking, not time: no active loan/transaction/finalizer/COMMIT/observer is
preempted, and work inside a holder remains the caller's responsibility. Foreground request
latency and commit-inclusive occupancy require separate measurements. In the retained 9038
sample 2029.5ms is SEND acquisition wait; 7ms is a batched operation counter ending at savepoint
release and excludes shared finalizers and COMMIT. `blocked_by` samples the holder label at
enqueue, not the complete holder history. Neither counter identifies a continuous blocker.

Hook installation remains a configuration barrier. It waits for earlier foreground work and
background loans up to its captured admission watermark, then installs while owning the sole
writer. When the barrier reaches the foreground head, it drains only background loans through
that watermark; later background loans cannot prolong that drain. The barrier can wait for its
earlier backlog; it intentionally does not inherit ordinary foreground priority. Prepare and finalize
cannot see different configurations within a managed transaction.

An abandoned borrower before acquisition returns the connection to the scheduler. If the
worker stops, pending foreground and background borrowers disconnect. No queued callback or
loan receives success before the existing commit/return boundary. Public raw connection access,
raw transaction control and source coverage restrictions are unchanged.

Tests use explicit held loans and channel handshakes for order/progress, cancellation, failure,
configuration barriers, rollback and observer ordering. The backlog growth control compares
12 and 12,000 background loans: selecting the queued foreground loan consumes exactly one
notification and one foreground head in both cases. This is scheduling work evidence, not
SQL or latency acceptance.
