# Merge queue test flakes

## Terminal tab protocol observation

`ui::terminal_tab::terminal_tab_protocols` appends Ctrl-B, Ctrl-A, Ctrl-B after each
input case and waits for the native program log to end in that barrier. Searching the
entire accumulated log can match across cases: the previous trailing Ctrl-B, an `a`
event in the current case, and the new barrier's first Ctrl-B. The kitty pattern accepts
other modifiers and event kinds for the middle `a`; legacy Ctrl-Shift-A encodes Ctrl-A.
That premature match starts before the current case and produces an empty byte slice.

A native run with the pinned PTY dependency reproduced empty Ctrl-Shift-A and key-repeat
rows even though the transparent transport tap and program log both retained the correct
bytes. Controlled split-barrier observations reproduce the premature match directly.
Both input and query observations now search only from the current case's input offset.
The committed byte matrix, native protocols, deadlines and product code are unchanged.
The earlier hosted empty key-release row is consistent with this overlap, but its summary
alone cannot prove the transport boundary of that particular attempt.

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
inside one SQLite snapshot. A separate cheap HTTP smoke test seeds 51 definitions and
checks HTTP 200, 50 cards and a continuation cursor without timing. Performance continues
to measure route cost and latency.

The statement budget detects materializing every definition through per-card queries.
It does not catch a slow single query or bound all rows visited inside a statement.
The first-page route in `daemon_cost` guards SQLite work at two store sizes, and the
Performance job measures elapsed time. The counter is available in every unit-test profile:
`store::STATEMENTS_RUN` is exported under `cfg(test)`, and the `smallclaims` dev-dependency
enables its `test-support` counter independent of optimization level.

## Exec exit-code field gate wake-up

`broken_gates::a_terminal_exec_fails_its_unsatisfied_field_gate` intermittently
waited for its 60-second deadline even though its command exited with code 2.
The exec driver reports the provider result before its own wrapper process exits.
A reconcile pass awakened by that claim can still observe the wrapper running,
record that state, and go quiet. The later process exit is not a new graph claim.
Without a pending-gate poll, the next full pass can arrive after the fixture deadline.

Delaying the wrapper's `exit_group` syscall by three seconds reproduces this ordering
on merge-group source `575d461e555f8705b2fa679d8e631f5de645e65a`: the gate stayed
working after the process exited and recovered only at 64.99 seconds. A paused-time
regression checks a driver receipt followed by a quiet wrapper exit. This schedule
demonstrates the wake-up defect; hosted timeout logs alone do not prove every prior
timeout followed the same ordering.

Pending exit-code field gates now use the existing gate-runner poll for a locally
owned exec. The poll wakes reconciliation when the process ends; the predicate still
reads durable observations and selected-launch evidence to decide pass or failure.
Remote execs are not polled by a replica. The fixture deadline, failure reason, and
downstream-step assertions remain unchanged, with no test failure retry.

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

## Rejoining fleet comparison

`fleet::rejoining_under_a_new_name_reports_post_leave_history_as_divergent` waited
for equal envelope inventories and a settled graph comparison, discarded that response,
then queried the diff again. Replication can advance between the two CLI calls. The
second response can therefore have pending coverage and `graph.equal = null` even
though the wait observed a settled comparison. In merge-group run 37718777141 at
`5bf27196f40cb38686b86ae26b131e826e2faa33`, the first attempt failed on that null value
while the later `fleet wait` receipt still reported divergent projections; a retry passed.

The fixture now retains the response that satisfies the existing readiness predicate
and checks its graph divergence and differing tables. Admission warnings and rejection
by `fleet wait` remain independently checked. The predicate, timeout, replication
implementation, and divergence assertions are unchanged; there is no failure retry.
