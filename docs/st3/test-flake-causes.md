# Merge queue test flakes

The correctness suite must enforce observable results and deterministic work budgets.
Elapsed-time thresholds belong in Performance, where runner load and timing are measured
explicitly. Retrying a correctness test does not repair its cause.

## Mission first page

`client_v0_contract::missions_first_page_stays_under_100ms_with_thousands_of_definitions`
required the whole HTTP request to finish in 100 ms. The elapsed time included waiting
for blocking workers, CPU scheduling and filesystem reads on a shared runner. An unchanged
request could therefore fail under unrelated load.

The replacement,
`api::client_v0::tests::missions_first_page_has_bounded_queries_with_thousands_of_definitions`,
uses the same SQLite page read as the HTTP handler. It builds 3,001 definitions, requests
50 cards, checks continuation, and permits at most 310 SQL statements (six per card
plus ten page/attention/snapshot statements), using the existing thread-local counter.
Statements from other parallel tests cannot enter the count. The page read stays
inside one SQLite snapshot. HTTP envelope and cursor behavior remain covered by the
client contract tests; Performance continues to measure route cost and latency.

The statement budget detects materializing every definition through per-card queries.
It does not claim to bound all rows visited inside a statement or elapsed time.

## Concurrent quick-agent responses

`api::tests::retention_quick_concurrent_calls_share_the_response` used the generic API
unit fixture, which opens SQLite with `mode=memory&cache=shared`. Its independent readers
can encounter a table lock while the batched writer runs. The observed failure was
`database-locked` on `batches`, SQLite extended code 262 (`SQLITE_LOCKED_SHAREDCACHE`).
Busy timeouts do not give shared-cache memory databases WAL reader/writer semantics.

This test now opens a temporary file-backed store, matching the daemon's WAL configuration.
A twelve-party barrier releases all callers together. It still requires identical responses
for the same idempotency key and exactly one `intent.desired` claim. The transaction that
commits the declaration and completed response is unchanged; there is no error retry.

## Terminal invocation exit

`terminal_binding::fake_harness_receives_mail_and_exits_while_the_shell_survives_daemon_restart`
can lose the provider outcome when a terminal receipt is published before the blocking
provider task returns. Publishing `ended` fences the driver's own mailbox. The wrapper
can then report `driver-exit=2` for a successful provider or retain `running` for a failed one.

[The provider-exit ordering fix](https://github.com/compoundingtech/smalltalk/pull/1831)
holds the owned terminal exit event until the existing provider exit-report path runs.
That path publishes the terminal event and runtime exit. Real mailbox rejections remain
active while waiting. Forced success/failure schedules demonstrate this ordering bug;
the historical `stale-mailbox-session` logs do not contain enough runtime/harness receipts
to prove that every prior failure had this cause. This runtime correction stays in its
own PR and is not duplicated in the fixture fixes.
