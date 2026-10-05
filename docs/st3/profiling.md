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
Each retained connection can cache up to 8 MiB of pages, so raising retention trades memory
for avoiding repeated schema parsing and statement preparation. Profiling counts fresh
connections as `read connection opened`.

The bundled SQLite build uses `SQLITE_DEFAULT_MEMSTATUS=0` through the workspace Cargo
configuration, including Cargo-based Nix builds. SQLite's process-wide allocation statistics
are disabled by default, removing their shared allocator mutex; statement, process CPU and
I/O accounting remain available.
