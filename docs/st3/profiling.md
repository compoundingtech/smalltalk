# Profiling the daemon

A daemon that answers slowly is usually waiting, not computing: for the store's single writer, for
one of its read connections, or for SQLite. The daemon can account for its own time so that a slow
request names what it waited for and who held it.

`GET /v1/client/request-latency` reports completed response-envelope timings without enabling
profiling. Its `routes` array retains mixed route totals (`scope: "route"`) and adds independent
`scope: "work-action"` rows for POST claim, renew, progress, complete, fail, release and extend.
An action row has a static route such as `/v1/work/renew/{*subject}`, `method: "POST"` and
`action: "renew"`; it retains no subject, body or token. Action samples are subsets of the mixed
route totals, so do not add counts across scopes. The seven action buckets are independent of the
256 general-route limit.

`GET /v1/client/agents` also gets `scope: "agents-read"` rows, `method: "GET"`, with `read` set to
`first-page`, `fresh` or `continuation` (any page with a `cursor`). A `fresh=true` first page waits
up to two seconds for a refresh to publish at or after the read's own cut, including when the
refresher is idle. Only a failed latest attempt with no newer attempt in flight returns the
newest publication at once. Its original snapshot cut and publication time show its age.
The other two answer at once from a published roster. Count the fresh row against its own
target: the route total includes it.

`scope: "agents-roster"` rows (also under route `/v1/client/agents`) time where that wait goes,
each in its own last-512 sample, `duration_scope: "roster-stage"`. Stage `refresh` is one refresher
fold from the previous publication, `refresh-cold` one with no publication to start from, which
refolds every card from the log (the daemon's first complete roster, or after a chunked
replication projection, a trim or a repair forgot every kept reduction), and `refresh-admission`
its wait for roster admission (`population: "refresher-folds"`).
Stage `fresh-wait` is a fresh read's wait for a publication at its cut (2 s at most), and
`fresh-page` the page it then builds (`population: "fresh-reads"`). Stages are not requests: they
add no route or path rows.

`scope: "work-list"` rows (under route `/v1/client/work`) time a current-work read served from
the published work list, `duration_scope: "work-stage"`. Stage `fresh-wait` is a fresh read's wait
for a publication at its cut (2 s at most, `population: "fresh-reads"`); `page` a first page served
from a publication, and `actor-build` one whose read built its actor's order in that publication
(`population: "first-pages"`). They add no route or path rows. A fresh work read, which `st work ls`
sends, still counts in full, wait included, under the `GET /v1/client/work` route and its
`person-read` target: it has no `(fresh)` path or target of its own.

Each row's `count` is its completed-response count since this process started, including error
responses. Percentiles use its last at most 512 completions (`recent_count`), in whole milliseconds.
Renew and claim have separate counts and percentile samples. `duration_scope: "response-envelope"`
includes handler queueing, request work, durable admission and envelope serialization; it does not
subtract writer wait or report only a hierarchy component. This is server completion time, not
network delivery time. Abandoned and still-running requests have no sample. Existing long-poll
route durations include deliberate waiting.

## Targets and live windows

[`slo/targets.toml`](../../slo/targets.toml) holds the daemon's targets: reads a person waits on,
writes and acks, terminal attach to first screen, one SQL statement, any transaction or hold of
the writer, daemon CPU, and this member's database size. Every daemon builds the file in. CI's daemon_load reads the same file.
A target describes what we aim for and gates nothing by itself.

The request-latency read adds `targets` and `paths` to `routes`. Each target and each path keeps
rolling 1-minute, 5-minute and 1-hour windows with `count`, `p50_ms`, `p99_ms`, `max_ms`,
`over_target` and `over_target_share`. A path is `METHOD /route`, or `stream COLLECTION` for the
time from a subscription on the client collections socket to its first snapshot, conversation
page or terminal screen. A read the daemon relayed from another machine's owner counts toward
the target's `remote_windows` and its remote target. A request that asks to wait (`wait_ms` above
zero, or `wait=true`) is a long poll: it is kept under `METHOD /route (long poll)` and counts
toward no target. An agents list asked to be fresh (`fresh=true`, which `st agents ls` and
`st agents tree` always send) waits for one roster refresh by design but is still a wait a person
sits through, so it is the path `GET /v1/client/agents (fresh)` with its own `person-read-fresh`
target and counts toward no other: a first page is held to the 100 ms of `person-read`, a fresh
list to 300 ms. The `sql-statement`, `transaction` (`read`, `write`, `writer_hold`) and `cpu`
rows cover the store's statements, its read and write transactions, each hold of its single
writer, and the process's CPU in cores.

The `writer-hold` row names who held the writer, against the transaction target (`max_ms`).
It is always on, without `ST3_PROFILE_DIR`. A holder is the request route or background task
the asking thread worked for, with the request's client when known, or `(thread NAME)`; each
row also gives `at`, the code location that asked. A batch is charged to the write in it that
ran longest; a lent writer to its borrower. `in_progress` lists each writer's current hold,
oldest first, with its holder, start (`started_unix_ms`) and `held_ms`, so a stuck hold shows
before it ends. `holders` ranks up to 16 holders by writer time over the last hour (count, over
target, total and max). `long_holds` and `long_reads` keep the last 64 writer holds and read
transactions over the target in that hour, newest first; a long hold also gives its `kind`
(`batch` or `lend`), `writes` and `longest_write_ms`. `open_reads` lists up to eight read
snapshots open past the target now. `waiting` counts callers waiting for the writer and reads
waiting for admission, and `threads` counts this process's threads, in all and by name. `st
doctor` prints it as `slo/writer-hold`.

The `database` row is this member's store: `max_live_gb` for the pages in use (the file less its
free pages, which SQLite reuses before the file grows) and `max_growth_mb_per_day` for their net
daily growth, what is written less what checkpoints and retention free. The daemon samples the
store hourly, after its local trims, by reading three pragmas, and keeps two days of samples in
`meta` (`database_size_samples`), so a restart does not lose the growth. Growth is measured from
the newest sample at least a day old, or extrapolated from the oldest once there is an hour of
samples; `size` says over what span. Each sample also records the lengths of the main file and its WAL:
`physical_bytes` is their sum, what the store takes on disk (page counts include pages committed
to the WAL but not yet copied into the main file, so they are never added to the WAL), and
`physical_growth_bytes_per_day` compares it with the sample nearest exactly a day ago, within half
an hour of it, scaled to a day. It is never extrapolated. A sample dated in the future, after the
clock went back, is dropped. The newest sample is the one every reader uses
(`st3::slo::database_size`), so disk-filling conditions read the same ring. [Retention](retention.md) explains what keeps the store
within it.

The windows live in memory. Each is a ring of slots (12 of 5 seconds, 10 of 30 seconds, 12 of 5
minutes). Each slot is a sparse histogram with 16 buckets per doubling, so a percentile is
within a sixteenth of its sample and never above the window's exact maximum. The share over target
is counted exactly as each sample finishes. Nothing samples on a thread of its own. A window ages
out when it is next written or read, and a restart empties it.

With `[observations.otlp]` set, the daemon also sends the windows to the collector once a minute
as gauges, from a timer task that reads the same report as the request-latency read and does no
work on a request path. A minute the collector refuses is dropped, not replayed; the collector
holds the history. Each point carries `target`, `series` (`owner` or `remote` for a read target;
`reported` for a latency paired phones reported; `statement`, `read`, `write` or `writer_hold` for
the store), `window` (`1m`, `5m`, `1h`) and, on
latency, `stat` (`p50`, `p99`, `max`), under the resource's `host.name`. The gauges are
`st_slo_latency_ms`, `st_slo_over_target_ratio`, `st_slo_samples`, `st_slo_target_ms` (the
figure from `slo/targets.toml`, so one query sets a host's number against its target),
`st_slo_cpu_cores` and `st_slo_cpu_max_cores`. A window with no samples sends its count only. A
phone's live share is a minimum over foreground milliseconds, so it is not in those: it goes out as
`st_slo_live_share`, `st_slo_foreground_ms` and `st_slo_min_share_percent`. The task reads only
the targets, not every path's snapshot.

`st slo` lists the same targets with the account limits as one list, and `GET /v1/slo`, the daemon's own route
beside `/v1/usage`, serves it to the CLI. The phone and the web app read it through a client route
that comes later, with its capability, schema and generated clients. An objective is a target with windows and an error budget: a p99
target may have 1% of its samples over, so `LEFT` is how much of that 1% the last hour has not
spent; a max target has no allowance, so it is whole or empty; CPU and the database's size read
against their own ceilings; a phone's live share against the disconnected time its minimum
allows. A budget is an allowance that resets: each account's weekly and 5-hour limit, with `LEFT`
the part not yet used and a reset time. `st slo explain ID` says one row in plain words and `ID`
may be the start of an id. The list is computed on each read from the live windows and the limit
readings, and writes nothing.

`st slo chart ID --over 24h` draws what is left of one row across a period (`90m`, `24h`, up to
`7d`). The history is a local observation log, not the graph: once a minute the daemon appends the
remaining percent of every row to `<state>/slo-history/YYYY-MM-DD.jsonl` (UTC day) and deletes a
day after seven. A chart column is the lowest reading in its slice of the period, so a brief spend
shows; a column with no reading is a dot, which means the daemon was down or nothing was
measured, not that a budget was spent. `GET /v1/slo/history?id=&since_ms=&until_ms=&steps=`
serves the same columns. A node with a collector keeps the longer history there.

`st doctor` prints a `slo/NAME` line for each target and a line for each of the ten paths furthest
over their target in the last five minutes. A p99 target is missed in a window when more than 1%
of its samples are over. A max target is missed when any one sample is over. A miss is `info`,
not `warn`, so `st doctor --strict` does not fail on a target.

For a deployed latency receipt, retain the process/source identity and before/after count delta.
A low-volume tail can include requests from before the measurement window, and a restart resets
these in-memory counters. The endpoint alone supplies no queue-versus-work breakdown; the phase
profiling below supplies that context separately.

Person-ask reconciliation checks for retained `work.person-asked` claims on a read connection
after importing legacy asks. When there are none, it skips the writer queue so an empty stage
does not delay the rest of the reconcile pass behind unrelated writes. When asks exist, their
owner checks and cancellation still run in the writer transaction.

Set `ST3_PROFILE_DIR` to a directory in the daemon's environment, for example in a systemd drop-in
for `st3.service`, and restart it. Without it the accounting is off, and each hook costs one atomic
load.

| Variable | Effect |
| --- | --- |
| `ST3_PROFILE_DIR` | Turns profiling on and names the directory it writes to. |
| `ST3_PROFILE_SLOW_MS` | Writes a line for each operation slower than this. The default is 250. |
| `ST3_PROFILE_PTRACER` | Linux only. Lets any process of the same user trace the daemon, so a stack sampler such as `eu-stack` can attach on hosts that allow tracing only a process's own children. |

The daemon records each request route (`GET /v1/messages/page`) and each background pass (`task
reconcile-pass`, `startup open-store`) as one operation. For each operation it records:

- Its wall time and how long it queued before a thread began it.
- How long it waited for the store's writer, how long it held the writer, and which operation held
  the writer when the wait began.
- How long it waited for a pooled read connection.
- Its SQLite statements, with their count and time.
- The CPU time and I/O of the threads that worked for it, and its response size.
- Who called it: the harness bound to the caller and the command that sent it.
- Named spans, such as each reconcile stage, and counted notes, such as why a replicated
  projection replayed the graph from nothing.

It writes four files:

- `minutes.jsonl` gets one line a minute. The line holds process CPU and I/O, how late the async
  runtime woke a 100 ms timer, each operation of that minute ranked by cost with percentiles, and
  a table of callers by route.
- `slow.jsonl` gets one line for each slow operation, with the same detail.
- `totals.json` holds the totals since the daemon started.
- `in-flight.jsonl` samples unfinished work every ten seconds when at least one tracked
  operation is a second old. It keeps the oldest 32 labels/callers and ages, including
  queue wait; at most 4,096 weak registrations are retained. The current file and its
  single rotated predecessor are capped at 8 MiB each. Labels and callers are capped
  at 256 UTF-8 bytes with visible truncation flags.

Work on a thread that runs no named operation appears as `(unlabeled THREAD)`.

Collection socket reads name their work `stream collection/missions`,
`stream collection/attention`, `stream collection/agents`, `stream collection/work`,
`stream collection/glasses` and `stream collection/arrangements`. Their shared bounded
claim-kind check is `stream collection/invalidation`. These labels include cache lookup and
live agent overlays as well as rebuilds; a label's operation count is not a rebuild count.
The socket remains responsible for its own snapshot and change delivery. Equivalent session
authority and query windows share computation through a disposable per-Store cache, while each
read rechecks paired authority and receives its own SQLite snapshot fence. Admission for an
equivalent cached window waits asynchronously before starting its blocking snapshot reader;
the physical worker retains admission through cancellation. Cache locks only select or publish
immutable results. Each Store retains at most 64 windows, with 16 per session; query churn past
that session quota evicts only that session's inactive windows. Separate local-commit epochs
survive later ignored claims. Missions share a bounded 200-card fleet projection independent
of subscriber grants and limits; attention shares by effective person visibility and custom-form
compatibility. Authorization is checked before reuse, including grant expiry and scope loss.
Their cached projections record the captured clock and the earliest exact output transition,
including effective mission-preview leases, recently-ended mission membership and attention
admission times; a backward clock or reaching a transition rebuilds. Failed/cancelled missions
remain current through the inclusive 24-hour cutoff and leave one millisecond later. The
membership fence includes the page's lookahead so `has_more` is refreshed as well as cards.
Selection, mission `must_act` and previews share the captured read clock. Attention rows do not
age out: observer grace, final-step fault
aging and checkpoint waits already belong to the separate fault projection, not this collection.
Wake times and execution deadlines affect stored states through graph writes, not these cards'
read clock.
The socket's existing 30-second timer remains the delivery cadence for clock-only changes.
This cache covers the six collections named above.

A disposable local single-client cold-read comparison is available with:
`cargo test -p st3 --lib shared_windows_local_single_client_cost -- --ignored --nocapture`.
It compares uncached, cold cached and warm reads for 100 glass rows under Unix and paired
sessions, alternating case order and reporting aggregate wall/process CPU. Debug-profile
fixture results do not measure deployed paired-client CPU or production tail latency.

`cargo test -p st3 --lib mission_attention_projection_fixture_timing -- --ignored --nocapture`
compares 22 uncached projections with shared cold and 22 warm reads over 70 generated missions
and person steps, asserting projection parity. Its stage profile separates mission attention-run
selection, definition reads, run overviews, step previews and attention snapshot work. The
`perf_load` workload includes `collection missions` and `collection attention`: synthetic bursts
of 20 concurrent WebSocket subscribers alongside the existing busy request mix. These are
synthetic fixture measurements, not measurements of a deployed daemon or transport latency.

Conversation collection followers (`follow_conversation`) and standalone conversation sockets
(`conversation_stream_socket`) use the existing conversation page/change reader. Mailbox reads
already name `task mailbox-snapshot` and `task mailbox-update`. Attention's collection clock
handles grace and expiry without a claim; HTTP Home/now reads retain their route labels.
Terminal sockets (`terminal_stream_socket`) follow the shared latest-screen source and periodically
recheck their live fence; the existing fence span is `terminal/live-fence`. These are source
locations for investigating ongoing work, not an attribution of process CPU. Long-poll waits and
socket lifetime must be distinguished from the cost of a completed read.

On `/v1/client/collections/stream` (`st3.client.collections.v0`), a conversation
subscription additionally owns `stream conversation/first-frame` from accepted receipt
through its first successful data-frame send. Its `conversation/first-frame-total`
includes admission/scheduler delay, owner resolution, initial retries, page work and
outbox/send delay. `completion: finished` means the socket send succeeded, not that
the client rendered it; cancellation or replacement records `dropped`, not success.
The follower releases first-frame ownership before its intentional change long poll.
Async admission, owner resolution/relay, first-page and send spans record wall time
only. CPU and SQL are charged only during a future's poll or its physical blocking
worker, never across an async suspension. Native preparation/fold, bounded header,
stored-message overlay, page/frame preparation and serialization use thread-local
CPU/SQL spans. These overlapping spans are not additive. Labels are static and carry
no transcript, audio, text, session identifier or credential attributes. No public
request-latency rows or diagnostic fields are added.

Local opening captures the first page and replay boundary together, using the native
keyset fold and reusing its bounded head for the header. Remote opening retains the
two established owner reads (changes with no cursor and zero wait, then timeline);
profile those reads separately before attributing repeated fold work to local opening.
For representative receipts, pin the daemon build and capture five successful initial,
tab-switch and reconnect frames on the used collections route, with client receipt/render
time beside the internal profile. Record local versus relayed ownership and cold versus
warm source state; exclude intentional reconnect-offline time from server-work comparisons.
The isolated `conversation_first_frame_profile_lifecycle` test runs with
`ST3_PROFILE_DIR` and `ST3_PROFILE_SLOW_MS=0`; it proves successful completion and
wall-only async attribution before long-poll idle, not production performance.

A slow record's `completion` is `finished` when the request or task explicitly finished its
profile. If its owner exits without finishing, including a canceled HTTP request, the last
worker records it with `completion: dropped`. That record includes blocking work that continued
after cancellation, and its wall time ends when the last worker exits. It does not prove that a
response reached the caller. Work still running when the daemon stops has no completion record.

SQLite reports a statement's time from its first step to its reset, at millisecond resolution. A
query whose rows the caller processes one at a time includes that processing, so the outer query
of a nested loop looks slow. Statement times from concurrent operations overlap.

With OpenTelemetry export enabled, `st.reconcile_pass` and `st.reconcile_deadline` are
independent roots, even when called inside a request or an attached OpenTelemetry context.
A pass that continues after an isolated member failure or an unavailable PTY snapshot still
exports ERROR status. `st.fifo.depth` and `st.fifo.oldest_age` describe pending work only:
writer jobs leave the backlog before waiting to begin their transaction, and closing a writer,
conversation, or terminal-emulation receiver discards its pending observations.
Writer observations include foreground jobs, background loans and configuration fences, but not
internal background-ready notifications. Dispatch removes the selected job's timestamp, so a
foreground job overtaking a background loan does not erase the older loan's waiting time.

## Writer batching and accounting

The store has one writer connection. Foreground jobs are FIFO. Opt-in background
loans use a separate queue; a pending background loan can be overtaken by at most
eight foreground loan or group-commit turns. This bounds dispatch turns, not elapsed
time or SQL work inside a holder. Configuration fences retain their ordering.

Batched jobs run in one `BEGIN IMMEDIATE` transaction, each in its own savepoint.
The writer stops taking more jobs after 256 jobs, 50 ms, or a lent write, commits
once, then answers callers. A failed or panicking job rolls back only its savepoint;
a failed transaction begin or commit fails the batch. A caller hears success after
commit. A lent write manages its own transactions and returns the connection when
it finishes. With `synchronous = FULL`, batching shares one disk flush across the
jobs in that commit.

Under `ST3_PROFILE_DIR`, an operation counts the time before its writer job starts
as writer wait, and the job itself as writer hold. The batch `COMMIT` appears under
`(unlabeled st3-writer)`, whose statement count is the number of batches. Phase times
and the enclosing operation can overlap; they must not be added as independent
request durations.

## Raw terminal cold opens

Enable profiling on both the gateway and owner to separate admission work from transport work.
Client requests record `admission/queue`, `admission/authenticate`, `admission/snapshot`, and
`handler/queue`. Terminal operations record `terminal/live-fence`, `terminal/capability-lookup`,
and `terminal/capability-consume`; the same record carries SQLite statement, read-wait, and
writer-wait accounting.

The gateway's raw stream records `raw/route`, `raw/dial` (including Fabric route resolution),
`raw/tcp-dial`, `raw/peer-upgrade`, and `raw/peer-verify`. The owner's
`GET /v1/peer/raw-terminal` operation records `raw/peer-authenticate`,
`raw/owner-attachment`, and `raw/owner-open`. Peer upgrade includes the owner's admission and
open work, so these nested durations must not be added together.

Async spans measure wall time against their originating operation even when the future resumes
on another thread. They do not attribute another thread's CPU or SQLite counters; the owner-local
client request records provide that detail. Profiling remains off unless `ST3_PROFILE_DIR` is set.

Capability admission seeks the attachment's indexed hash and its subject-head fence in one
SQLite snapshot instead of decoding a bounded page of the whole fleet graph. Single-use CAS,
session, person, and mode binding, expiry, and owner/incarnation checks remain unchanged.

## Checkpoint scratch memory

Checkpoint capture filters envelope cuts in SQLite and sorts only claim identities and canonical
keys, then decodes retained bodies in bounded pages. Each metadata, body, protection and tombstone
page has its own read snapshot and checks the persistent capture invalidation epoch. Mutations
that affect the registered below-cut envelope prefix, canonical ordering, its protection references
or below-cut tombstones restart capture; three invalidated attempts fail closed with a clear error.
The seal rowid is fixed once per capture. A short atomic writer statement advances the monotonic
frontier and accepted-time cut only when their persisted bounds need to grow; retries and repeated
captures within covered bounds do not rewrite them. Admission of newer above-cut envelopes,
identical duplicate re-offers and projection writes referring only to newer history do not
invalidate capture. Delayed admission into the captured prefix and repairs that protect captured
claims still invalidate it. The WAL is released between pages rather than pinned across sorting
and the full body pass. The default and maximum capture page sizes are 64 envelopes/records.
The compact metadata pass strictly decodes claim acceptance timestamps before envelope exclusion;
malformed, negative or overflowing values return an error rather than silently counting as early.
Capture mutation guards seek batch and record membership through their existing indexes, then
look up the exact envelope identity. They do not scan the retained envelope frontier for each
deleted claim during trim. The capture mutation guards preserve the cut/frontier and invalidation rules.
Mission-run and planning replay retain the canonical list of IDs and load one claim body at a
time. Base replay uses a temporary ID order and bounded body pages. Both close their ordering
or body statements before projection savepoints.

Proof copies use a disk rollback journal and temporary storage, with a fixed 2 MiB SQLite cache
target. The scratch directory needs space for the database copy and its rollback journal.
This does not change the sealed set, canonical ordering, proof digests, or reader-equivalence
checks. Large retained sealed sets still consume memory; reader page-cache settings do not
bound decoded claim bodies or allocator retention. Compare live allocations and SQLite's
allocator counters with RSS before attributing an RSS plateau to retained read connections.

## Writer worker admission

Writer-bound API handlers, replication receive, mailbox control and daemon maintenance
wait asynchronously before entering Tokio's blocking pool. Each writer admits at most 32
top-level synchronous writer-bound callbacks by default. Mutation handlers run as admitted
future polls: when a body, service, mutex or receipt wait returns pending, its blocking worker and
writer permit return before the future waits asynchronously for its next wake. Set
`SMALLCLAIMS_MAX_WRITE_WORKERS` in the **daemon's** environment to a positive integer to
override this count; zero, invalid, or values beyond Tokio's semaphore capacity use 32. The value is read once per process.

Admission holds no SQLite connection or store lock. Dependent nested callbacks share their
parent's capacity, including when a current-thread runtime needs a separate blocking child.
Started callbacks retain capacity through completion or panic even if their awaiting task
is dropped. API mutations keep their existing completion and acknowledgement behavior.
Non-writing publication/git preflight, gate startup, attestation and terminal control effects
run independently of writer admission; their mutation continuation admits again before
polling store work. The request and result receipts still surround each terminal effect.
Read workers and health checks have independent capacity. The existing writer queue still
orders foreground jobs, drains fences and schedules background loans; admission does not
change a callback's loan class. Synchronous/raw writer access remains available and does not
become a process-wide thread bound.

This bounds the thread multiplier while a writer is held; it does not shorten that hold
(#2185) or budget retained reader caches (#1381/#1936). Tokio's global blocking limit stays
unchanged: native provider sessions occupy blocking threads for their lifetime and may wait
on daemon control, and current-thread nested callbacks require another blocking slot.
A global cap needs a separate dependency-safe design.

The isolated Linux `smallclaims` example from issue #2636 preserves a raw control path:

```sh
cargo run -p smallclaims --example blocking_pool -- 128 --writer-only
cargo run -p smallclaims --example blocking_pool -- 128 --writer-only --admitted
```

Omit `--writer-only` to run the original raw-reader cache phase too. That phase deliberately
bypasses admission so reader retention can be compared independently.

## Read connections and SQLite allocation

Top-level API read queries admit before opening a connection or pinning a snapshot.
Waiting for reader capacity holds no cache, roster, or socket gate; waiting for those gates
holds no reader capacity. Each pool admits at most 32 top-level workers by default. Set
`SMALLCLAIMS_MAX_READ_WORKERS` in the **daemon's** environment to a positive integer to
override this count; zero, invalid, or values beyond Tokio's semaphore capacity use 32.
The value is read once per process. Queued reads observe their existing cancellation and
deadline budgets without opening a reader. An upgraded stream's query is admitted without
adding a deadline policy; waiting for events or socket/window admission holds no reader.
Collections retain their FIFO reader grant into the physical query instead of discarding
it and racing a new try-acquire. Other gates are tried without waiting; contention releases
capacity and unrelated gates before waiting for the contended gate.
Same-pool nested reads reuse their loan, including pinned snapshots. Cross-pool nested
reads never wait for the inner pool's capacity while holding an outer loan or snapshot:
they try spare capacity, otherwise use an unadmitted nested loan. Status fanout tries
extra capacity without waiting, then reduces inline on the parent's reader if none is free.

After admission, reads take an idle connection or open another when all retained connections
are busy. The pool still retains up to 128 idle connections by default;
`SMALLCLAIMS_MAX_IDLE_READ_CONNECTIONS` overrides only that retention ceiling. Admission
does not change page-cache targets, statement caching, or the allocator. Legacy raw pool
checkout, non-API background reads, and the cross-pool nested fallback remain ungated:
32 is a top-level worker-admission bound, not a global physical connection count or a
process memory limit.

Each reader requests a fixed 8 MiB page-cache target by default. Set
`SMALLCLAIMS_READ_CACHE_KIB` in the **daemon's** environment to override it in KiB; positive
integers through 2,147,483,647 are accepted, and invalid or zero values use the default.
The value is read once per process. Retention defaults to 128, preserving schema and prepared-statement reuse.
Profiling counts fresh connections as
`read connection opened`.

Each reader keeps up to 128 prepared statements. Mailbox changed-since, owner/binding,
local watermark, and snapshot fence checks reuse those statements; caching removes repeated
SQL preparation. Mailbox streams now use a local dependency index: one dispatcher reads new
claim subjects and local observations from the existing post-commit feed and wakes only streams
depending on their seat, message subjects or recipient. Local binding commits wake the affected
owner/component directly. This routing state is disposable and never replicated.

Streams subscribe before their first read and retain the durable changed-since and fencing
checks. A full safety read every thirty seconds with a randomized per-binding phase recovers missing
dependencies, missed notifications and ownership changes. Initial dispatcher-cursor failures never replay from zero: they retry at
the current head and resync subscribers once. Batch failures retain their cursor and retry every
five seconds. A warning log and the `mailbox-wake-dispatcher` doctor check expose failures since
startup and whether routing is currently degraded. There is no debounce. `task mailbox-wake-dispatch`,
`task mailbox-change-check` and `task mailbox-snapshot` expose the routing, checking and full-read
costs separately in doctor and profiling counters. Run the isolated many-stream comparison with
`cargo test --release -p st3 --lib api::mailbox::profile::many_streams -- --ignored --exact --nocapture`;
`ST_MAILBOX_PROFILE_DIR` retains its generated store, latency/CPU results, doctor counters and
profile. Its load client and daemon run in separate processes.

Status reachability checks walk the sparse `operations_conflict_index` and seek matching
claims through `claims_operation_index`. An unrelated idempotency conflict must not turn
every subject's status read into a scan of its historical JSON bodies. The operation ID's
TEXT affinity is removed in that join so SQLite can seek the JSON-expression index.

The agent-card cache holds its mutex only while selecting or publishing immutable cached
rows, not while building cards. HTTP agent pages and WS roster windows first probe read-only
caches inside a fresh, authorized SQLite snapshot. An exact, lease-valid warm hit bypasses the
store-wide roster admission even while an unrelated cold reader holds it. A miss releases its
snapshot before awaiting admission, then opens a new snapshot and rechecks authority and caches.
Cold HTTP first pages build shallow refs and selected cards under one admission and one snapshot;
warm first pages need neither admission nor a second snapshot. The existing same-key WS window
admission remains in place for authority fencing. The cold worker retains store admission through
completion even if its caller disconnects, so waiting followers pin no old WAL read mark.

Eight immutable graph-index/local-frontier/history cuts are retained. HTTP pages lazily fill missing subjects into
the same projection used by the complete WS roster, without reducing unrelated cards. Local
agent observations and registered agent claims update only their subject's card. Run and generation
changes refresh owned cards and diff roster membership; step/work changes map owners, assignees,
claimants and activity actors. Queue-affecting claims refresh fleet queue selection and selected
labels once, then compare all queue fields per card before refolding. Message sends refresh their
agent endpoints because they contribute to activity timestamps. All message lifecycle receipts
also refresh projected agent parties: draining rollout blockers read the message's delivery state,
including ancestors of eligible replies. Receipt claims need not contain endpoint fields.
Daemon diagnostics, glasses, arrangements and fleet-only claims reuse the cards. Unknown kinds and
unclassified structural changes conservatively cold-build. Authorization is checked before reuse,
and local delivery presence stays a per-read overlay rather than graph-cached authority.

The reuse allowlist includes transitive rollout, suspension, fault and placement dependencies:

| Claims | Card read | Delta mapping |
| --- | --- | --- |
| `message.sent` | activity and rollout delivery blockers | projected agent sender/recipient |
| `message.staged`, `message.delivered`, `message.read`, `message.closed` | rollout delivery blockers and eligible reply ancestry | projected agent sender/recipient; cold if no message projection exists |
| `daemon.diagnostic`, `daemon.started` | none; diagnostic reports are separate from agent reconcile faults | empty, guarded to daemon subjects |
| `glass.upserted`, `glass.deleted` | none; glass projection is not a card or blocker input | empty, guarded to glass subjects |
| `arrangement.edited` | none; arrangement projection is not a card or blocker input | empty, guarded to arrangement subjects |
| `fleet.invite-created`, `fleet.invite-redeemed`, `fleet.invite-revoked` | none | empty, guarded to invitation subjects |
| `fleet.member-admitted`, `fleet.member-endpoints`, `fleet.member-left`, `fleet.member-removed` | none; host metadata is declaration-derived and placement fences do not read peer liveness | empty, guarded to host subjects |

Shared cards are built only from current, independently time-fenced queue metadata. A
pagination continuation applies its frozen ordering/host/queue refs to the response clone
after reading the shared cards; frozen pagination cuts must never seed the shared projection.
The shallow HTTP membership/order/queue refs have the same bounded graph-cut retention.
Warm pages reuse those refs rather than scanning all fleet work. An explicit allow-list of
existing-agent harness observations and daemon diagnostics leaves them valid; every other
claim rebuilds them. Runtime status can move an undeclared or stopped agent into history,
so even an existing agent's `runtime.observed` rebuilds shallow refs.
Both shallow refs and the full-card projection expire at the earliest current-generation
work-lease deadline. Expired work becomes ready (or disappears from a revision-draining queue)
without a new claim. Full-card refreshes retain expired rows only as diff sources, never as warm
hits: `roster/queue-diff` scans queue selection and labels once and refolds only cards whose queue
fields changed. It also refreshes the next deadline. Shallow refs still rebuild after expiry.
Safe card-local advances carry the prior deadline forward. Shallow-ref warm hits add no SQL;
full-card probes
read the relevant local frontier before selecting cached rows. The outer shared agents
WS window inherits the same fence, so it cannot hide an expired full-card projection.
HTTP reads see expiry immediately; live WS subscribers see the transition on the next
existing 30-second authority/freshness tick. That existing live-stream bound is unchanged.

Historical status cuts read queue and declaration presentation from the caller's current SQLite
projection, just as cold cards do. When the requested graph index precedes that physical
projection's frontier, full-card reads bypass shared cached rows and cold-build selected coverage;
they do not borrow stale queue fields from an earlier physical snapshot.

The isolated 60-agent, six-mission, 60-step fixture counts canonical claim kinds and local samples:
`cargo test -p st3 --lib agent_roster_typical_claim_kind_baseline -- --nocapture`.
Its frozen pre-incremental classification distinguishes the old cold fallback from the current
selected-card build count. Full JSON parity, including queues, todos and usage, is checked by
`agent_roster_seeded_claim_sequences_match_cold_json` with reproducible proptest sequences,
both history modes, selected coverage, historical cuts and injected-clock lease expiry.
The ignored `agent_roster_one_step_claim_fixture_timing` reports cold/incremental wall times and
the number of refolded cards after one work claim; run it explicitly with `--ignored --nocapture`.
These debug-profile fixtures are not deployed latency or loaded-host benchmarks.

The relevant local-observation frontier is the largest agent `harness.timeline` row ID visible
inside the same SQLite snapshot with `after_store_index` at or before the requested graph cut,
or zero when none exists. A partial covering index, `local_observations_roster_frontier_index`,
contains only those rows. The reverse lookup skips newer heartbeat rows entirely, without
assuming row IDs and graph indices are monotonic; an old cut still steps over newer agent
timeline rows in the index. Existing stores create the index automatically on their next open,
without a schema-version bump or replay migration. Initial creation took 0.171 seconds on
an offline 1.78 GB store copy with 161,048 local observations (118,115 indexed agent timeline
rows), using SQLite 3.53.3. The 8,701-row synthetic fixture took 0.516 ms; these isolated
index-creation timings do not measure end-to-end startup or predict other stores.
Heartbeat-only `harness.observed`, local telemetry and non-agent timeline appends leave cached
cards reusable. Every agent timeline entry type advances the frontier, including usage entries,
because managed-session recency reads the latest timeline row even when last activity does not.
Replicated status and usage changes retain their graph-index invalidation. A same-index local
content transcript append misses the read-only getter
and updates the affected card's `last_activity_at` when rebuilt for both HTTP and WS; ignoring
timeline rows here would make a shared warm roster instant but stale. Read-only getters never
build, require selected-subject coverage, and pin immutable rows without filtering or cloning them.

Cache relevance does not filter the existing `changed()` notification signal. Local
heartbeat and timeline publications still wake every existing event subscriber; a heartbeat
can then reuse the roster rows, while timeline activity misses and refreshes them. The normal
`agent_roster_cache_reuse_preserves_changed_for_existing_subscribers` regression checks both
publications with two subscribers.

The performance report exposes each roster stage under bounded task labels:
`roster/admission-wait` (waiting for the shared admission or an in-flight build),
`roster/frontier-read`, `roster/cache-hit`, `roster/build` (incremental advance or cold build),
`roster/card-projection` (refolding changed cards) and `agent_work_queues`.
With trace export enabled, HTTP reads of a published complete roster or startup head report
`st.roster.mode=hit` and the served publication's card count on their request span. Rebuilds
remain independent roots of the background refresher, not children of the reading request.

The focused 70-agent, 20-session fixture reports card-status, usage, repeated-projection and
incremental-update costs with `cargo test -p st3 --lib agent_roster_snapshot_fixture_timing --
--ignored --nocapture`. It is a serial fixture micro-measure, not a load benchmark. CI's
`perf-load` workload holds concurrent agents WS subscribers and measures first-snapshot
latency against the roster's 300 ms budget.

Normal tests include a small cached-versus-uncached roster comparison before and after local
timeline activity, in both history modes. Run the heartbeat-only fixture with
`cargo test -p st3 --lib agent_roster_heartbeat_only_local_stream_hit_rate -- --nocapture`:
it prints append/probe/hit counts, hit rate and elapsed time, requires every heartbeat probe to
reuse the same immutable rows, and requires a subsequent timeline append to miss before rebuild.
The normal warm HTTP fixture
`warm_agent_http_page_bypasses_unrelated_cold_roster_admission` holds store admission while reading
a cached first page and prints its elapsed time. These are deterministic cache/admission
regressions with diagnostic timings, not production latency guarantees.

Every five seconds a dedicated native thread attempts a passive WAL checkpoint outside the
writer queue. Once every frame is backfilled, it attempts `TRUNCATE` with zero busy timeout.
Active readers or writers defer recycling; the daemon never waits for them while holding the
writer queue. The ordinary 1,000-page SQLite auto-checkpoint remains enabled. A passive copy
can take time on slow storage, but does not acquire the writer lock. Open failures retry;
caught panics reopen the connection after a one-minute cooldown. Worker error logs are
limited to one per minute. The detached thread is not a Tokio blocking task, so runtime
shutdown does not wait for a long backfill.

`st doctor` reports open, idle and active reader counts, peak open readers, total connections
opened since startup, the configured cache target, and summed current reader targets without
requiring SQLite MEMSTATUS. These are targets, not measured allocations: SQLite schema,
prepared statements, query results and allocator overhead are additional. The report names
the configured API read-worker admission bound. Raw reader checkout remains non-waiting;
background reads and retained idle connections can exceed the admitted-worker count.

The doctor planning envelope uses the larger of current open readers and idle retention,
multiplied by the per-reader target, plus the writer's 32 MiB target and a 512 MiB reserve for
schema/statements, projections, tasks and allocator overhead. At defaults this is 1,568 MiB:
1 GiB for 128 retained reader caches, 32 MiB for the writer and the 512 MiB reserve.
The 1 GiB cap used in diagnostic examples is illustrative, not the service default:
the Home Manager module defaults `memoryMax` to `"8G"` and installations can override it.
For an installation limited to 1 GiB, set `SMALLCLAIMS_READ_CACHE_KIB=2048` to restore
the 800 MiB planning envelope, or adjust its service limit.
The reserve is a planning allowance, not an enforced limit or a guarantee for every graph
or workload. On Linux, doctor
locates the daemon's own cgroup v2 mount and checks `memory.max` and `memory.events` in that
cgroup and its visible ancestors. It warns when the tightest limit is below the planning
envelope, any `max` counter records pressure, or the files cannot be inspected. Ancestor
counters include other descendants; historical hits do not prove a current OOM. On other
platforms it reports that cgroup diagnostics are Linux-only.

Checkpoint application and proof completion make one best-effort allocator-reclamation
attempt on Linux with glibc (`malloc_trim(0)`). Nested entrypoints defer that one call until
the outer operation has dropped its temporary inputs and SQLite guards. Successful,
reader-mismatch, and error completions share the cleanup; skipped agreement/proof work
does not trigger it. There is no periodic trim, idle-WAL hook, or ordinary API-read hook.
Other targets use a no-op. This releases only unused glibc pages, not live SQLite caches
or retained application data, and is not a process-memory cap. The call may contend with
concurrent allocator activity; compare its duration as well as post-operation RSS.

Reproduce reader-cache multiplication with invented data and the bundled SQLite artifact:

```sh
cargo run -p smallclaims --example reader_memory -- 8192 32 3
cargo run -p smallclaims --example reader_memory -- 2048 32 3
```

Compare mailbox, work, interactive mission and replication API reads in fresh isolated
processes using the existing generated benchmark store (scale 0.1):

```sh
cargo build -p st3 --features test-support --example reader_cache
target/debug/examples/reader_cache /tmp/reader-cache-fixture --generate-only
SMALLCLAIMS_READ_CACHE_KIB=8192 target/debug/examples/reader_cache /tmp/reader-cache-fixture
SMALLCLAIMS_READ_CACHE_KIB=2048 target/debug/examples/reader_cache /tmp/reader-cache-fixture
```

CPU and RSS in this tool cover the API daemon process; its Python client runs in a separate
process. Compare identical builds and workloads; debug-build latency is not a production
latency promise. It prints per-wave CPU, latency, RSS and reader counts. Only generated
invented data and private sockets are used.

The bundled SQLite build uses `SQLITE_DEFAULT_MEMSTATUS=0` through the workspace Cargo
configuration, including Cargo-based Nix builds. SQLite's process-wide allocation statistics
are disabled by default, removing their shared allocator mutex; statement, process CPU and
I/O accounting remain available.

A full replication projection fallback always writes one bounded line to daemon stderr before
replay starts, even without `ST3_PROFILE_DIR`. It names the phase, reason, previous frontier and
entry target, for example `st: projection full replay phase=startup/project-replication-backlog
reason=missing-health frontier=0 target=1200` (one physical log line). Reasons distinguish missing
or unhealthy health, a frontier ahead of the log, a non-incremental kind, a work operation,
malformed operation metadata, an operation conflict, and `incremental-error:CODE`. Healthy
incremental chunks produce no fallback log. The target is the admitted index observed at entry;
a full replay can also include claims admitted since that observation.

The daemon reserves both Unix socket addresses right after acquiring its startup lock, before
opening SQLite, but does not listen on them. A socket that is bound and not listening makes the
kernel refuse every connection at once, so clients and `st doctor` keep seeing "daemon starting"
with the current phase, and nothing queues behind an uninitialized router. The daemon starts
listening only when migrations, admission, canonical projection catch-up and application
initialization have finished. A per-address advisory lock (`<socket>.reservation.lock`) is held
from reservation through serving, so a second daemon neither unlinks a reserved address nor a
live listener; the lock is released when the process exits.

Reserving early moves the path preparation (live-listener probe, stale-file removal, bind,
permissions and discovery link) off the ready-to-listen interval. It does not preserve an exited
process's established streams or remove the stop-to-bind refusal window. Zero-refusal restarts
require a service-manager-owned socket that survives the writer handoff, with the replacement
accepting only after SQLite is ready.

### Startup phase durations

Every successful daemon start emits one structured stderr line when both listeners are ready:
`st3: startup phases {"configuration_ms":...,"install_hooks_ms":...,"open_store_ms":...,...,"total_ms":...}`.
The service journal captures this line without enabling profiling. Durations use a monotonic
clock and milliseconds; `total_ms` starts at entry to daemon initialization, not process execution.
Failed starts that never reach listener readiness do not emit this summary.

The phase keys partition that interval: `configuration_ms`, `install_hooks_ms`,
`open_store_ms`, `judge_claims_ms`, `validate_replication_backlog_ms`,
`apply_replication_repairs_ms`, `settlement_projection_ms`, `runtime_prelude_ms`,
`login_environment_ms`, `isolation_pty_ms`, `recorder_ms`, `app_state_ms`,
`reconciler_ms`, `background_tasks_ms`, and `bind_ms`. Configuration includes identity and
instance-lock setup; hooks include profiler initialization and the file-descriptor limit.
Opening the store includes signing-key setup. Settlement and projection share one interval;
the runtime prelude includes the startup claim, notification state, and PTY directory.
`bind_ms` adds the early socket reservation to the interval that ends at the second listener's
readiness callback. The summary adds no per-request
instrumentation and is not repeated for projection chunks or readiness progress updates.

## Reproduce a mailbox-wake comparison

`scripts/profile-mailbox-wakes` copies the current test-only fixture into a disposable checkout
of the requested ref. Production code comes from that ref; the added module is `cfg(test)` only.
The script removes the checkout after the run and retains build identity, logs, store, doctor and
CPU/latency evidence. Run it inside the pinned development shell with a new output directory:

```sh
nix develop --command scripts/profile-mailbox-wakes HEAD /tmp/mailbox-targeted
nix develop --command scripts/profile-mailbox-wakes HEAD /tmp/mailbox-idle-100 --idle 100
nix develop --command scripts/profile-mailbox-wakes HEAD /tmp/mailbox-idle-250 --idle 250
```

The idle mode first opens all 256 streams, then seeds 100 or 250 pending messages per seat, waits
for every delivery stream to show its complete mailbox, and measures 60 seconds without graph
writes. CPU covers the daemon process, including safety reads and heartbeats; setup and doctor
are excluded. The payload is 352 bytes per message. Providers and the reconciler are absent.
This is an explicit synthetic sizing probe, not an observed production unread distribution.

## Browser perceived-UX traces

The browser exports `service.name=webfractal-web` to the same-origin OTLP relay. Dogfood pages
record **100%** of budget roots and SDK detail.
No local storage, transcript content, resource identifiers, credentials, or free-form error
details enter production spans. The existing 200-span `SpanRing` is the dependency-free local
assertion surface; adding the separate otelite package is unnecessary for these lifecycle tests.

| Span | `span.label` | Attributes and measured boundary |
| --- | --- | --- |
| `wf.ux.reload` | `roster` | Navigation start → committed shell paint → committed roster paint. `wf.ux.shell_ms`, `wf.ux.roster_after_shell_ms`; q96 `wf.ux.budget_ms=100`, `wf.ux.budget_phase=roster-after-shell`, strict `<100ms` `wf.ux.budget_met`. |
| `wf.ux.switch` | `warm` or `cold` | Selection → transcript DOM commit followed by paint. `wf.ux.cache`, `wf.ux.slot_count`, `wf.ux.data_ready_ms`, `wf.ux.painted`; `wf.ux.budget_ms=100`, `wf.ux.budget_phase=selection-to-paint`, `wf.ux.budget_met`. |
| `wf.ux.switch.data_ready` | `first page` | Child of switch: selection → first decoded page, separately from painted completion. |
| `wf.ux.first_frame` | Collection name, `conversation`, or `terminal` | One bounded root per subscribe attempt: actual subscribe send → first verified frame, not paint. `wf.subscription.kind`, `wf.ux.early`, `wf.ux.outcome`; `wf.ux.budget_ms=30000`, `wf.ux.budget_phase=subscribe-to-first-frame`, `wf.ux.budget_met` express the bounded first-frame lifecycle. |
| `wf.sync.transition` | `window`, `conversation`, `terminal`, `gateway` | SyncStatus v2 tag/stage/reason transitions; `wf.subscription.kind`, `wf.sync.from`, `wf.sync.to`, `wf.sync.stage`, `wf.sync.previous_ms`, `wf.sync.reconnect_ms`. Span duration is capped at 30 seconds; full dwell and reconnect durations remain numeric attributes. `wf.ux.budget_ms=30000`, `wf.ux.budget_phase=transition-bound` describes this lifecycle bound, not a product reconnect SLO. |
| `wf.ux.send_echo` | `echo` | Feature-owned hook: send → verified echo DOM commit followed by paint; `wf.ux.painted`, `wf.ux.budget_ms=100`, `wf.ux.budget_phase=send-to-echo-paint`, `wf.ux.budget_met`. |
| `st3.capabilities`, `st3.snapshot`, `st3.messageSend`, `st3.follow.subscribe` | Fixed operation name in production | SDK children inherit the active UX context and propagate their own child traceparent; identifiers and failure details are stripped. |
| `st3.socket.probe`, `st3.socket.create` | `probe`, `socket` | Per-attempt capability probe and credential/socket issuance. These are children of an active UX operation, or independent roots on reconnect; creation is not a long-lived socket span or a handshake timing claim. |

UX operations have a 30-second deadline and `wf.ux.outcome` distinguishes `painted`, `observed`,
`timeout`, `superseded`, and `disposed`. Repeated progress ticks do not create new transition
spans. The 100ms switch/echo comparison is a telemetry budget, not evidence that those feature
surfaces meet it.

### Feature paint and echo hooks

The live `ConversationPane` owns the transcript and composer. Selection starts a
conversation follow and emits `wf.ux.switch.data_ready`; data publication alone is
not paint. The visible pane observes its committed transcript content and calls
`telemetry.ux.transcriptCommitted(ref)`, canceling that observation during effect
cleanup. Only that boundary finishes the switch root with `wf.ux.painted=true`.
Absent a committed boundary, the root ends as timeout/superseded/disposed.

For a composer, call `telemetry.ux.beginSendEcho()` at send. Its `committed()` callback belongs
on the verified matching echo DOM commit, never on the HTTP acknowledgement; call `cancel()`
when the operation is abandoned. No echo span is fabricated by the data layer.

### Propagation and early roster connection

SDK HTTP requests use the generated-client-compatible `traceContext` callback. Browser
WebSocket upgrades carry validated W3C context in trace-only URL parameters (browser WebSocket
APIs cannot set request headers); the gateway strips those parameters before forwarding,
parents its request span to the browser context, and forwards its child context upstream.
Subscribe frames carry optional `trace: { traceparent, tracestate? }`; older daemons ignore
unknown frame fields. Frame-level daemon parenting requires the corresponding daemon protocol
support; upgrade-level and HTTP parenting do not.

The roster bootstrap runs before the main module and sends its first subscribe without waiting
for the bundle. The SDK adopts the page-owned socket and buffered frames, without persistence,
duplicate roster subscribe, or a second initial socket. Its first-frame observation is an
independent root linked to the bootstrap trace, rather than adopting an unrelated active UX parent.
Browser fixtures exercise early-send ordering and committed-paint completion; SDK/server tests
assert trace-ID continuity.

On `pagehide`, the source synchronously closes its collection socket before starting asynchronous
disposal. Custom SDK sockets receive code 1001; native browser APIs prohibit that reserved code,
so their adapters use no-argument close (an empty close frame, reported as 1005 by a server).
A persisted (back/forward-cache) page retains its scope; persisted `pageshow` reconnects and
resubscribes its existing interests. These lifecycle handlers add no timers. The early unclaimed
socket also closes synchronously on pagehide.
The document entry owns a single lifecycle handler: close the socket, synchronously unmount
React registry consumers, then dispose the source. This explicit ordering does not rely on
capture/bubble ordering between separately registered window listeners.

The browser proof commands require `playwright-cli` on `PATH` and the locked workspace
dependencies installed:

```sh
node apps/fractal-web/scripts/early-connect-proof.mjs
node apps/fractal-web/scripts/ux-paint-proof.mjs
```

The early-connect proof controls module delivery, then imports the actual main entry,
renders the real shell, and checks that the SDK took the one initial socket without sending
a second roster subscribe. It then dispatches persisted page-transition events to exercise
retained-scope reconnect, and performs a real browser reload. The test server requires each
close frame before the next roster subscribe. It expects an empty native close frame on simulated
pagehide; real navigation may additionally use the browser's own 1001 close. The persisted events
test the handlers,
not browser-specific back/forward-cache eligibility. This proves the early path while a bundle
is loading; it does not promise that every network's WebSocket handshake outruns an already-cached
bundle. Both commands close their ephemeral servers and browser sessions in `finally`.

The daemon's existing [`CollectionSubscribe`](../../crates/st3/src/api/client_v0.rs)
derives Serde deserialization without `deny_unknown_fields`, so the optional trace field
is accepted and ignored by older servers.

