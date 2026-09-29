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

## Read lanes

The store reads through five lanes, so a read waits only for work in its own lane. Every
connection opens its own files, and the daemon runs under a default limit of 1024 open files, so
the lanes share sixteen connections:

| Lane | Connections | Reads |
| --- | ---: | --- |
| Critical | 2 | Seat message polls (`GET /v1/messages/page`), one message, one subject's status (`GET /v1/status?subject=`), and the admission of every client request: its credential and its graph snapshot. |
| Operational | 4 | The agent list and agent details, which divide one projection across the four connections of the lane. |
| Projection | 2 | Projections rebuilt from history on every request: every subject's status or a status history, a work item's detail, sessions, machines, and the missions tree. At most two such requests run at once. The rest wait for a turn before they take any connection. |
| Interactive | 4 | Every other request. |
| Background | 4 | The reconciler and other daemon tasks. |

A projection divides across connections only in a lane of four. In a lane of two, only one read
at a time may pin a snapshot, so a worker thread could wait forever for the pin its caller
holds.

On hetz on 2026-09-29, before the projection lane existed, four concurrent work-item details held
all four interactive connections for 36 seconds. Every seat's message poll waited 5.0 to
5.8 seconds for a connection, and a client request spent 35 seconds being let in. Now those
details queue among themselves. A test holds every projection connection with more details
waiting than may run, and message polls, one subject's status, `client/now`, the agent list,
attention and the work list still answer within 250 ms.

The event feed and session timelines are not projection reads, although they read history. They
can wait up to 30 seconds for news or for another host, and a waiting request must not hold a
turn.
