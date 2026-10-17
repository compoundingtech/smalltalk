# Writer admission and non-writing waits

Issue #2636 limits the callbacks that can park waiting for the live store's writer.
Admitting a whole HTTP mutation handler was too broad: 32 callers waiting for a body,
service or mutex could occupy all 32 permits before touching SQLite and block a later
mutation. The revised mutation driver admits each synchronous future poll before
`spawn_blocking`. A pending poll returns the blocking worker and its permit; the owned
future then waits asynchronously for its waker before another admitted poll.

A dependent writer callback retains its shared permit until that callback actually
finishes, including on a current-thread runtime. Dropping a request waiter does not
cancel its detached mutation. Non-writing external callbacks use an explicit separate
blocking path; they must not borrow the live store's writer. Read admission and health
remain independent. There is no global blocking-thread cap.

## POST and PUT audit

The router is in `crates/st3-api/src/api.rs`. Both PUT routes are `/v1/notes` and
`/v1/client/glasses/{id}`. Every JSON POST/PUT, and the Bytes extractor on
`POST /v1/client/blobs`, can await a slow request body before doing any write. These
extractors now return pending polls without retaining writer capacity or a blocking
parent. This applies to all routes, without a list of body exemptions or a new timeout.

The following paths contain additional non-writing waits that can exceed a second.
They may have 32 concurrent waiting callers; none keeps a writer permit during the
listed wait. No concurrency assumption about callers is needed.

| POST path or family | Non-writing wait | Admission during that wait |
|---|---|---|
| `/v1/github/watch`, `/v1/github/comment`, `/v1/github/own` | Credential lookup and GitHub reads/posts/reviews | Async HTTP/auth awaits return pending; writer admission returns. Local watch/receipt writes execute in a later admitted poll. |
| `/v1/client/actions` | Idempotency mutex, remote workspace/screen/control/attachment relay, first local terminal screen | Mutex, relay and screen awaits return pending and release admission. Action gates are separate from writer capacity. |
| `/v1/client/actions` importing a native session | Wait for the exact predecessor process to stop | `blocking_external` runs termination without writer capacity. The parsed/fenced takeover still orders stop, declaration and start. |
| `/v1/sessions/input/{*subject}`, `/v1/sessions/{subject}/context/clear`, `/v1/sessions/{subject}/signal`, and the corresponding client actions | PTY/provider or exec control effect | `blocking_external` runs only the effect. The durable request receipt precedes it; the result receipt follows in another admitted poll. |
| `/v1/sets/preview`, `/v1/sets/apply`, `/v1/intent/mission`, `/v1/intent/apply`, `/v1/work/mission/{*subject}`, and launch/create paths calling `publication_refusals` | Workspace/git publication preflight | The non-writing preflight runs via `blocking_external`; the parent poll returns admission while awaiting it. Publication writes remain admitted. |
| `/v1/checkpoint/plan` | Planning and proving on a scratch copy | Live sealing/capture is admitted. Planning and scratch-copy proof run via `blocking_external`, with no live writer capacity retained. The same disk-staged set, staged planner/proof, response fields and outer allocator-completion boundary are used. Dry-run staging keeps main's separate `checkpoint/plan` directory and cleanup on proof success/error. |
| `/v1/gate-checks` | Environment/filesystem/process startup | Gate startup runs via `blocking_external`. The handler returns the initial check; it never waits for exec gates to finish. |
| `/v1/sekrets/attest` | Process/scope and ancestry inspection | Attestation runs via `blocking_external`, without writer capacity; its live-store access is read-only. |
| `/v1/internal/client-read/forward` forwarding terminal control | Owner/control relay | Async relay waits return pending without capacity. Forwarded read variants already have a read budget and retain their separate read path. |
| `/v1/client/observations` | Body/report processing | This existing envelope bypass takes no writer permit. Its separate `BUSY` ticket limits report processing to one. |

Other mutation handlers publish local graph changes, capabilities, receipts or local
state and do not wait for a provider operation, peer response, replication convergence,
a mission/step to finish or a receipt from a future mutation. Agent restart, rollout,
suspend/resume and launch endpoints record requests and notify the reconciler; their
HTTP handlers do not wait for the requested lifecycle transition. Mailbox bind/receipts
and harness-event publication commit their local result instead of awaiting delivery.
Replication receive, heal and checkpoint-adopt handlers perform local store work and
return its receipt; they do not await a peer, replication convergence or a later
writer mutation. Replication export is already on the budgeted read path (its artificial
delay exists only under `test-support`) and takes no writer admission permit.

A synchronous admitted local-store callback can still take more than a second while
executing SQL or computing its result. That productive work intentionally occupies one
of the bounded workers; the limit does not promise a wall-time or throughput bound.
The liveness distinction is that an async service/body/mutex wait or a non-writing
external effect does not keep a worker, and cannot require a later writer mutation to
free the writer admission it holds. Local slow/broken work must be fixed at its cause;
this change adds no retry, timeout or cached response to hide it.

## Regression

The actual response-envelope tests create 32 POST handlers parked on a controlled
service notification, then issue a 33rd mutation. The mutation must commit before
releasing any notification. Another pair of tests keeps 32 JSON POST bodies pending
and requires the 33rd mutation to commit before supplying those bodies. Both cases
run on multithread and current-thread runtimes. Cleanup releases every waiter before
reporting a failure, so the old whole-handler boundary fails without leaking blocked
workers. The existing 128-writer burst and independent read tests remain required. Two further
tests drop a mutation waiter while its service future is pending, prove that its only
writer slot is free, then release the service and require the detached mutation to commit. Tests use notifications and future wakes, not polling timers.

The read long-poll path still has its legacy blocking parent around `block_on`.
Read-event/screen/log waits are separate from writer admission, and thread names or
futex counts alone do not establish a live stack attribution. This change makes no
claim to eliminate that independent source of blocking threads or reader memory.
