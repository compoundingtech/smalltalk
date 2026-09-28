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

## Follow-up work

Measure handler queue time, read-pool acquisition time, per-subject projection time, writer-lock acquisition time, and reconcile-pass duration in the daemon. Move expensive synchronous projections and reconciliation off Tokio workers while preserving snapshot consistency and existing graph guarantees. Reduce repeated per-subject queries or cache a snapshot keyed by store index, then retest with the copied graph and concurrent read/write requests. Keep the timeout as a failure signal until the slow path is removed.
