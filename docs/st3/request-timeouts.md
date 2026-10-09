# Request stalls on a copied production sized graph

## Reproduction

On 2026-09-28, I used SQLite's online backup API to copy the active claim store into a separate state directory. I ran the installed `st3` binary against that copy with a distinct node, Unix sockets, and PTY root, with no peers configured. The isolated daemon did not start any of the original host's agents. The copied graph had 186,807 claims, 328 agent subjects, and 68,266 claims for those subjects. Its highest claim index stayed at 186,936 throughout the measured read load, so the reproduction did not require concurrent writes.

All timings below are wall time for complete CLI calls to the isolated socket. Three serial calls established the idle baseline:

| Call | Idle range |
| --- | ---: |
| `agents ls` | 0.661–0.716 s |
| `missions ls` | 0.056–0.082 s |
| `conversations ls` | 0.007–0.013 s |
| `work ls` | 0.011–0.014 s |

With 24 concurrent `agents ls` calls, those calls took 10.58–17.04 s. One call each to `missions ls`, `conversations ls`, and `work ls`, started one second after the load began, took 10.45 s, 9.99 s, and 9.72 s respectively. The daemon consumed 24.49 CPU seconds over that 17.11 s window. In a separate 16-call `agents ls` batch, each call took 7.40–8.95 s. A 32-call mixed batch (24 agent listings and eight mission listings) took 20.88 s; several agent listings took more than 20 s and the mission listings took 13.68–14.18 s. The CLI calls in these isolated batches succeeded, so the measured reproduction is request delay beyond the reported 15 s limit, rather than a reproduced CLI timeout response.

During a separate 24-call load, 30 samples of the daemon's Linux thread states at 200 ms intervals found 617 sleeping in `futex_do_wait`, 131 runnable, and two in other states, across 750 thread observations. Samples typically showed 18–21 futex sleepers and 3–7 runnable threads. Linux ptrace policy prevented attaching `strace` to attribute each futex to a particular lock.

## Cause and limits of attribution

`client_agents` in `crates/st3/src/api.rs` calls `client_agent_resources` synchronously on a Tokio worker. That function calls `status_for_subject_prefix_at("agent/")`, which enumerates subjects and calls `status_at_view` separately for each. Each subject reduction runs multiple SQLite lookups and graph projections. `Store` has four read connections guarded by a mutex and condition variable (`READ_CONNECTIONS = 4` in `crates/st3/src/store.rs`). The agent listing also scans work queues and looks up per-agent reconcile faults. The prefix enumeration itself uses an index; a standalone count over the copied graph took about 15 ms, so the repeated per-subject reduction is the expensive part.

The causal chain supported by this reproduction is that concurrent, synchronous graph projections occupy Tokio workers and the four-connection read pool. Other API handlers then wait for worker time and/or a read connection even when they request different resources. The futex samples and the four-connection limit support lock or pool waiting, but do not identify the exact share of each request spent on the pool condition variable versus other mutexes. The unchanged claim index and all-read workload rule out SQLite writer contention or a long write transaction as necessary causes of this isolated stall. The original host's high load and reconciliation may add delay; this test does not apportion that extra delay.

The reconciler's `run` loop also invokes synchronous `reconcile_once` directly in an async Tokio task. A long pass can therefore occupy another API worker; its duration was not separately instrumented in this experiment. The read-only reproduction already explains why unrelated calls can queue behind expensive requests without a reconcile write.

## Fix and verification

The agent list now builds its status and cards once for each graph index and history choice. Concurrent calls share that build. A cache entry is reused across daemon diagnostic claims because those claims cannot change agent cards. Its fallback `updated_at` value is filled from each request's own snapshot timestamp. A new graph index still rebuilds the list, and historical indexes retain their own cache entries. A large bounded status projection divides the agent subjects across the four existing read connections, then merges the results in subject order. Agent list handlers run the synchronous projection on a blocking worker, leaving Tokio workers available to admit other requests. The response envelope records a `daemon.diagnostic` fault with code `slow-request` when a request takes at least one second.

On a fresh isolated backup with 201,147 claims, a cold agent listing took 1.62 seconds before dividing the status scan and 0.74 seconds after. A cold batch of 24 agent listings plus one each of missions, work, and messages completed in 0.91 seconds. Agent calls took 0.887–0.894 seconds; the other calls took 0.238–0.543 seconds. A warm batch of the same shape completed in 0.46 seconds, with agent calls taking 8–22 ms. A profiling run before the parallel scan attributed 1.26 seconds of a cold 1.27 second agent projection to the status reduction, with work queues and card assembly taking about 11 ms together. The tests exercise cache reuse with all read connections occupied, graph-index invalidation, ordered reduction of more than 64 subjects, asynchronous handler responsiveness, and durable slow-request fault recording.

The cold batch has less headroom than the warm batch; a larger graph or higher CPU pressure may still exceed one second, and those requests will raise durable faults. Reconciliation duration and writer-lock wait were not measured by the read-only reproduction.

## Read connections since 2026-09-30

A fixed pool made reads wait for one another. On 2026-09-29, a fleet host's store had four
connections in each of four classes. Four work-item reads held every interactive connection for
36 seconds. Every seat's mailbox poll waited 5 to 6 seconds behind them, and a client request's
admission waited 35 seconds.

Now a read takes an idle connection, or opens another when every one is busy. A read never
waits for another read, and in WAL mode never for the writer. The pool keeps up to 32 connections
between reads, each caching up to 8 MiB of pages, and closes the others. A read waits for a
connection only when the operating system refuses to open one, so the daemon raises its soft
open file limit toward the hard one, up to 8,192, when it starts. A large status projection still
splits its subjects across four threads, each on its own connection at the same store index.

`GET /v1/replication/status`, `GET /v1/internal/fleet/status` and
`GET /v1/internal/fleet/membership` read the replication snapshot of envelopes already sealed,
built on a read connection. Sealing this node's newest batches into envelopes needs the writer,
so the exchange paths do it, and a batch written since the last exchange shows in these reads
after the next one.

## Writes since 2026-09-30

One thread owns the managed writer connection. Durable writes queue in arrival order.
Replaceable current registers use a separate connection with zero busy timeout and a 100 ms
transaction deadline. They never queue or retry: contention drops that sample. A register
commit can briefly hold SQLite's write lock while a managed write attempts admission; budgeted
callers can receive `database-busy` and retain their existing retry policy. Reads remain read-only.
Durable peer errors and `replication_refusals` still use the managed writer.

Each register commit uses its own `synchronous=FULL` flush. On an isolated test host, the native
register probe measured 7.0–13.4 ms from `BEGIN IMMEDIATE` through `COMMIT` (including flush),
versus 16.1–22.6 ms for the complete publication call. Fleet-scale throughput and request
latency are checked by the load harness; these isolated numbers are not throughput guarantees.

- **Batched writes.** Claims, legacy local observations, work actions, step and run
  state changes, publications (`apply`), documents, resource observations, mission outputs and
  the receipt of a peer's envelopes each run as one job. The writer thread runs the next job and
  the jobs queued behind it in one `BEGIN IMMEDIATE` transaction, each in its own savepoint. It
  stops taking jobs after 256, after 50 ms, or at a lent write, commits once, and then answers
  every caller. A job that returns an error or panics
  rolls back only its savepoint; the others still commit. If the transaction cannot begin or
  commit, every job in it gets that error. A caller hears success only once its write is on disk.
- **Lent writes.** A write that manages its own transactions, such as admitting and projecting
  replicated envelopes, checkpoints and repairs, borrows the connection in its turn in the queue
  and gives it back when it finishes. Jobs queued before it commit first.
- **Async workers stay free.** Message sends and lifecycle posts wait for their commit on a
  blocking thread, as claims and work actions already did, so a queued write holds no async
  worker that other requests need.

With `synchronous = FULL` each commit waits for a disk flush. Batching shares one flush among
every write that arrived while the previous batch ran.

Under `ST3_PROFILE_DIR`, a batched write's operation counts the time until the writer started
its job as writer wait, and the job itself as writer hold. The batch's `COMMIT` shows under
`(unlabeled st3-writer)`, whose statement count is the number of batches.
