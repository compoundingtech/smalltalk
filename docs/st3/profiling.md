# Profiling the daemon

A daemon that answers slowly is usually waiting, not computing: for the store's single writer, for
one of its read connections, or for SQLite. The daemon can account for its own time so that a slow
request names what it waited for and who held it.

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

It writes three files:

- `minutes.jsonl` gets one line a minute. The line holds process CPU and I/O, how late the async
  runtime woke a 100 ms timer, each operation of that minute ranked by cost with percentiles, and
  a table of callers by route.
- `slow.jsonl` gets one line for each slow operation, with the same detail.
- `totals.json` holds the totals since the daemon started.

Work on a thread that runs no named operation appears as `(unlabeled THREAD)`.

A slow record's `completion` is `finished` when the request or task explicitly finished its
profile. If its owner exits without finishing, including a canceled HTTP request, the last
worker records it with `completion: dropped`. That record includes blocking work that continued
after cancellation, and its wall time ends when the last worker exits. It does not prove that a
response reached the caller. Work still running when the daemon stops has no completion record.

SQLite reports a statement's time from its first step to its reset, at millisecond resolution. A
query whose rows the caller processes one at a time includes that processing, so the outer query
of a nested loop looks slow. Statement times from concurrent operations overlap.

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

## Read connections and SQLite allocation

Reads take an idle connection or open another when all retained connections are busy. The
pool retains up to 128 idle connections by default; `SMALLCLAIMS_MAX_IDLE_READ_CONNECTIONS`
overrides that retention ceiling. It does not cap concurrent connections or make reads wait
for an available slot. Nested reads and pinned snapshots keep their existing behavior.
Each reader requests a fixed 2 MiB page-cache target by default. Set
`SMALLCLAIMS_READ_CACHE_KIB` in the **daemon's** environment to override it in KiB; positive
integers through 2,147,483,647 are accepted, and invalid or zero values use the default.
The value is read once per process. Retention still defaults to 128, preserving schema and
prepared-statement reuse from #1234. Profiling counts fresh connections as
`read connection opened`.

Each reader keeps up to 128 prepared statements. Mailbox changed-since, owner/binding,
local watermark, and snapshot fence checks reuse those statements; caching removes repeated
SQL preparation but does not change the graph-wide wake fan-out.

`st doctor` reports open, idle and active reader counts, peak open readers, total connections
opened since startup, the configured cache target, and summed current reader targets without
requiring SQLite MEMSTATUS. These are targets, not measured allocations: SQLite schema,
prepared statements, query results and allocator overhead are additional. Reader checkout
remains non-waiting; **burst concurrency is unbounded**. Bounded admission must first resolve
cross-thread pinned snapshot dependencies (#1381).

The doctor planning envelope uses the larger of current open readers and idle retention,
multiplied by the per-reader target, plus the writer's 32 MiB target and a 512 MiB reserve for
schema/statements, projections, tasks and allocator overhead. At defaults this is 800 MiB,
leaving 224 MiB beyond that reserve under a 1 GiB service cap. The reserve is a planning
allowance, not an enforced limit or a guarantee for every graph or workload. On Linux, doctor
locates the daemon's own cgroup v2 mount and checks `memory.max` and `memory.events` in that
cgroup and its visible ancestors. It warns when the tightest limit is below the planning
envelope, any `max` counter records pressure, or the files cannot be inspected. Ancestor
counters include other descendants; historical hits do not prove a current OOM. On other
platforms it reports that cgroup diagnostics are Linux-only.

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
