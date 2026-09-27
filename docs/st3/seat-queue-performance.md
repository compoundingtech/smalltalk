# Seat queue performance

This document compares the idle cost of the seat queue branch with the st3 commit it last merged.
It records the method, the fixture, the numbers, one fix made because of them, and what the
memory numbers do and do not measure.

## Verdict

**Not yet.** The evals pass, apart from one Claude start failure and one void run, but the 9-hour
overnight run shows the branch using more idle CPU and waking far more often than base. Merge after the extra wakeups
are explained, or accept the cost knowingly.

- **For merging:** the seat queue works with live agents. 16 of the 18 runs last night on Claude,
  Codex and omp seats passed. One Claude seat never reached idle before any work existed. One omp
  run is void because the agent re-declared its own seat, and `16aa754` now refuses that. No seat
  claimed out of order, preempted a held claim, or used terminal input. The reads are cheap.
- **Against merging now:** after about an hour, the branch daemon's voluntary context switches
  rose from about 6,000 to 10,000–12,000 a minute and stayed there. Its thread count went from 20
  to 22, and base peaked at 21. Base stayed at about 6,500 a minute. Idle CPU was 45 s/hour against
  41 on base (+9%). The 30-minute windows below could not see this, because it starts later. I
  have not found the cause.
- **Memory growth is not the branch's.** Resident memory grew 11 to 13 MiB an hour on both builds
  for the whole night. The branch ended 22 MiB higher (232 against 210 MiB).
- **The measured build is not the branch head.** The overnight run used `2ee767c`. `37ac4b2` and
  `16aa754` add agent queue moves and the seat declaration check, and were not measured overnight.

## Result

- **Idle CPU is not higher in 30-minute windows after one fix.** The branch as first measured used 3.3% more idle CPU
  than base when the graph took six writes a minute, because the reconciler read every seat's
  order on each loop. `d2c4ee8` reads a seat's order only when the seat must choose between runs.
  After it, idle CPU is 0.357 against 0.354 s/min with read-only traffic and 0.622 against
  0.623 s/min with writes. A reconcile after one write costs 43.7 to 44.3 ms of CPU against 43.3
  to 44.4 ms on base.
- **Live memory is the same.** Both builds hold 38.1 MiB of heap in use after startup.
- **Resident memory under glibc is higher in most starts.** The fixed branch settled 33 MiB
  (about 6%) above base in 18 of 26 starts, and base never did (0 of 28). The extra is freed
  startup heap that glibc keeps. `malloc_trim(0)` takes both builds to 60 MiB, and one idle thread
  started at load puts the fixed branch at base's level. I did not find a branch change that
  causes it. By this step's rule that is measurably higher RSS, so the branch passes on CPU and on
  live memory, and does not cleanly pass on resident memory under glibc.
- **Under a jemalloc preload the difference is within noise.** Three starts of each read 125.6 to
  129.9 MiB on base and 124.8 to 143.3 MiB on the fixed branch.
- **The seat queue reads are cheap.** All twelve seat orders take 1.07 ms in process, one seat's
  order 0.18 ms, and the agent queue read 0.8 ms of daemon CPU. `st3 agents queue` takes about
  4 ms from the CLI, mostly process start.

Series 4, a 9-hour side-by-side run, found what a 30-minute window cannot: see the verdict.

## Builds

| Name | Commit | What it is |
| --- | --- | --- |
| base | `9b3c0a3` | the st3 commit `agent/seat-queue` last merged |
| branch | `ec7a3d9` | the seat queue branch as first measured |
| fixed branch | `d2c4ee8` | the branch after the reconcile fix below |

Each is a `cargo build --release -p st3 --bin st3`. The base source came from `git archive`, and
its release artifacts were built before any branch release artifacts existed. Cargo hashes
workspace crates by relative path, so both builds cannot share one release directory without a
clean between them.

## Fixture

`scripts/st3-seat-queue-perf/fixture` builds one graph through the public API of an isolated
daemon running the branch build. Every window then starts from a fresh copy of that claim store.

| Part | Size |
| --- | ---: |
| Claims | 100,000 |
| Durable seats | 12 top-level command seats, each a real PTY running `sleep` |
| Live runs with seat steps | 52 (60 started, 8 cancelled) |
| Seat queue entries | 76 across the 12 seats, 4 to 7 per seat |
| Queue moves | 300, deterministic, over every placement |
| Moved runs later cancelled | 8, still named by moves |
| Messages | 600, five of six archived |
| Filler | harness timeline claims up to 100,000 in all |

Each seat has two kinds of run. A direct run has a ready step for the seat. A relay run first
waits on the next seat, so every queue also has waiting runs to pass over. The live daemon on the
measuring host had just over 104,000 claims at the time, so this is at live scale.

The base build does not know the `agent.queue.moved` claim kind. It stores those claims and
ignores them. `Store::open` rebuilds the operation and planning projections from the claims on
every start, on both builds. On this store that takes about 15 seconds, and the copy grows from
478 MB to 1.1 GB plus a 700 MB WAL.

## Method

`scripts/st3-seat-queue-perf/series` alternates base and branch windows, one daemon at a time.
Each window:

1. copies the fixture store and starts one isolated daemon under `nice -n 10`. The daemon has
   its own config, sockets, PTY registry, and state directory, no peers, and a clean login
   environment. It uses the default glibc allocator unless `PERF_DAEMON_ENV` adds a preload;
2. replays what twelve idle native drivers send (`traffic.py`): one mailbox page per seat each
   second, and each minute that seat's status and work list;
3. skips a 5-minute warmup, then samples the daemon once a minute for 30 minutes
   (`sampler.py`): CPU time from `/proc/PID/stat`, context switches summed over every thread,
   RSS, and peak RSS;
4. stops the traffic and runs the probes (`probe.py`): 30 seconds of quiet daemon CPU, then 200
   of each read, and 200 claim writes spaced 250 ms apart. Each write wakes the reconciler, which
   runs until the graph is quiet. Probe CPU is daemon CPU minus the quiet rate.

The first series used read-only traffic. The live daemon wrote about six claims a minute while
idle, and each write wakes the reconciler, so the second series also writes six timeline claims a
minute (`PERF_WRITES_PER_MINUTE=6`). The third series repeats read-only traffic for base and the
fixed branch.

`scripts/st3-seat-queue-perf/startup` starts each build from a fresh copy of the fixture with no
traffic and reads its memory 5 and 60 seconds after the socket opens. With `PERF_TRIM_SHIM` it
preloads `trim.c`, which on a signal writes glibc's `malloc_info` and calls `malloc_trim(0)`.
That separates heap in use from heap that was freed but not returned to the system.

The host has 16 cores. Another st3 daemon ran its own idle soak on the same host during every
window, so absolute numbers include that background. Both builds saw the same background, in
alternating windows. No cargo build ran during a window.

`crates/st3/tests/seat_queue_perf.rs` times the store reads in process against a copy of the
fixture store.

## Series 1: read-only idle traffic

Base `9b3c0a3` against branch `ec7a3d9`, 30 measured minutes per window.

| Window | CPU s/min, mean (sd, max) | Context switches/min | RSS MiB, start / end / max | Peak RSS MiB |
| --- | ---: | ---: | ---: | ---: |
| base 1 | 0.352 (0.009, 0.37) | 5,934 | 574.1 / 574.2 / 574.2 | 590.7 |
| branch 1 | 0.353 (0.009, 0.37) | 5,982 | 577.3 / 577.5 / 577.5 | 590.6 |
| base 2 | 0.356 (0.011, 0.38) | 6,003 | 574.1 / 574.2 / 574.2 | 590.3 |
| branch 2 | 0.360 (0.010, 0.37) | 6,035 | 577.8 / 578.0 / 578.0 | 591.2 |

| Probe, mean latency and daemon CPU per operation | base 1 | branch 1 | base 2 | branch 2 |
| --- | ---: | ---: | ---: | ---: |
| Quiet daemon, no traffic | 0 ms/s | 0 ms/s | 0 ms/s | 0 ms/s |
| Mailbox page for one seat | 0.30 ms, 0.25 ms | 0.22 ms, 0.20 ms | 0.26 ms, 0.20 ms | 0.24 ms, 0.20 ms |
| Work list for one seat | 0.96 ms, 0.95 ms | 0.94 ms, 0.90 ms | 0.94 ms, 0.90 ms | 0.93 ms, 0.90 ms |
| Roster read | 534 ms, 534 ms | 550 ms, 549 ms | 543 ms, 543 ms | 541 ms, 540 ms |
| Agent queue for one seat | n/a | 0.81 ms, 0.75 ms | n/a | 0.82 ms, 0.80 ms |
| Claim write and the reconcile it wakes | 18.2 ms, 43.4 ms | 18.0 ms, 45.6 ms | 17.5 ms, 42.4 ms | 18.2 ms, 45.9 ms |

Idle CPU is the same: 0.354 s/min on base and 0.357 s/min on the branch, within the spread between
windows of the same build. Peak RSS is the same. Steady RSS was 3.5 MiB higher on the branch in
both of its windows.

A claim write and the reconcile it wakes cost 2.3 to 3.5 ms more CPU on the branch, about 6%.
Read-only traffic never wakes the reconciler, so this series cannot show that cost in idle CPU.
At the live rate of six writes a minute it would add about 15 ms of CPU a minute.

### Cause

The reconciler reads seat orders on every loop:

- to find its next wake deadline, it read the order of every local seat;
- for each seat with a ready harness, each pass read that seat's order again.

The fixture's command seats publish no harness state, so only the first read ran here. A read
replays the seat's moves. In process, on the fixture:

| Read, 200 times each | Mean | p95 |
| --- | ---: | ---: |
| `seat_run_orders`, all 12 seats | 1.073 ms | 1.099 ms |
| `seat_run_order`, one seat | 0.179 ms | 0.191 ms |
| `seat_queue` view, one seat | 0.509 ms | 0.526 ms |
| `agent_work_queues`, the roster queues | 1.174 ms | 1.206 ms |

A rerun at `d2c4ee8` read 1.122, 0.189, 0.546 and 1.204 ms, within 5% of these.

Most of the time the order cannot change the answer. It matters only when a seat holds nothing,
has ready work in more than one run, and can be woken.

### Fix

`d2c4ee8` reads a seat's order only in that case. `seat_chooses_between_runs` decides it from the
work the reconciler has already loaded. The wake deadline also requires a harness that can be
woken. `reconcile::tests::seat_order_is_read_only_when_the_seat_chooses_between_runs` covers it,
and the existing seat queue tests still pass.

Every seat in the fixture has ready work in more than one run, but none can be woken, so the fixed
branch reads no seat order here. A seat that can be woken, holds nothing, and has that choice still
costs about 1.1 ms a loop for the wake deadline, which reads every seat's order and keeps the
choosing ones, and 0.18 ms a pass for its own order. That lasts only until the seat claims its
next step.

## Series 2: idle traffic with six writes a minute

Base `9b3c0a3` against fixed branch `d2c4ee8`, then one window of the unfixed branch `ec7a3d9`,
30 measured minutes per window.

| Window | CPU s/min, mean (sd, max) | Context switches/min | RSS MiB, start / end / max | Peak RSS MiB |
| --- | ---: | ---: | ---: | ---: |
| base 1 | 0.624 (0.018, 0.65) | 6,169 | 585.4 / 600.2 / 600.2 | 600.2 |
| fixed branch 1 | 0.621 (0.021, 0.66) | 6,183 | 589.6 / 600.7 / 600.7 | 600.7 |
| base 2 | 0.621 (0.022, 0.68) | 6,134 | 587.0 / 602.4 / 602.4 | 602.4 |
| fixed branch 2 | 0.622 (0.021, 0.66) | 6,171 | 589.6 / 599.7 / 599.7 | 599.7 |
| unfixed branch | 0.643 (0.021, 0.68) | 6,173 | 596.0 / 624.0 / 624.0 | 624.0 |

| Probe, mean latency and daemon CPU per operation | base 1 | fixed 1 | base 2 | fixed 2 | unfixed |
| --- | ---: | ---: | ---: | ---: | ---: |
| Mailbox page for one seat | 0.26 ms, 0.20 ms | 0.22 ms, 0.20 ms | 0.30 ms, 0.30 ms | 0.23 ms, 0.20 ms | 0.25 ms, 0.25 ms |
| Work list for one seat | 0.95 ms, 0.95 ms | 0.91 ms, 0.85 ms | 0.96 ms, 0.90 ms | 0.92 ms, 0.85 ms | 0.90 ms, 0.85 ms |
| Roster read | 548 ms, 548 ms | 552 ms, 552 ms | 542 ms, 542 ms | 554 ms, 553 ms | 601 ms, 600 ms |
| Agent queue for one seat | n/a | 0.82 ms, 0.80 ms | n/a | 0.80 ms, 0.80 ms | 0.84 ms, 0.80 ms |
| Claim write and the reconcile it wakes | 17.4 ms, 44.4 ms | 17.4 ms, 43.7 ms | 18.5 ms, 43.5 ms | 18.4 ms, 44.2 ms | 18.1 ms, 46.2 ms |

RSS in MiB at the start of each minute, from daemon start, with every fifth minute shown:

| Window | 0 | 5 | 10 | 15 | 20 | 25 | 30 | 35 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| base 1 | 567 | 585 | 591 | 591 | 594 | 594 | 595 | 600 |
| fixed branch 1 | 567 | 590 | 595 | 598 | 600 | 600 | 601 | 601 |
| unfixed branch | 571 | 596 | 599 | 602 | 613 | 622 | 622 | 624 |

With writes, idle CPU is 0.623 s/min on base and 0.622 s/min on the fixed branch. The unfixed
branch used 0.643 s/min. That is 3.3% more, about five standard errors of a 30-minute mean, so
the unfixed branch was measurably higher and the fixed branch is not. A reconcile after one write
costs 43.5 to 44.4 ms of CPU on base, 43.7 to 44.2 ms on the fixed branch, and 46.2 ms unfixed.

Writes make the heap grow during a window. At the daemon's first sample, every base window read
566.7 to 566.8 MiB and every fixed branch window 566.8 to 567.0 MiB. Every unfixed branch window,
in both series, read 570.8 to 571.0 MiB. The fixed branch ends at 599.7 to 600.7 MiB, against
600.2 to 602.4 MiB for base. The unfixed branch ends at 624 MiB.

So the steady 3.5 MiB of series 1 was set at startup. Startup runs many reconcile loops while the
seats' runtimes start, and the unfixed branch read seat orders on every one of them. In the
start-up trials below, the unfixed branch's extra 4 MiB sits outside the main heap: 7.8 MiB of
other anonymous memory against 3.8 MiB on base and the fixed branch.

## Series 3: read-only idle traffic, fixed branch

Base `9b3c0a3` against fixed branch `d2c4ee8`, read-only traffic as in series 1, 30 measured
minutes per window.

| Window | CPU s/min, mean (sd, max) | Context switches/min | RSS MiB, start / end / max | Peak RSS MiB |
| --- | ---: | ---: | ---: | ---: |
| base | 0.354 (0.008, 0.37) | 5,939 | 574.1 / 574.2 / 574.2 | 590.7 |
| fixed branch | 0.357 (0.012, 0.37) | 6,031 | 607.2 / 607.3 / 607.3 | 607.3 |

| Probe, mean latency and daemon CPU per operation | base | fixed branch |
| --- | ---: | ---: |
| Quiet daemon, no traffic | 0 ms/s | 0 ms/s |
| Mailbox page for one seat | 0.24 ms, 0.25 ms | 0.27 ms, 0.20 ms |
| Work list for one seat | 0.94 ms, 0.90 ms | 0.91 ms, 0.90 ms |
| Roster read | 534 ms, 534 ms | 550 ms, 550 ms |
| Agent queue for one seat | n/a | 0.81 ms, 0.80 ms |
| Claim write and the reconcile it wakes | 18.3 ms, 43.3 ms | 17.6 ms, 44.3 ms |

Idle CPU matches series 1 on both builds. Base repeated its series 1 memory exactly. The fixed
branch held 33 MiB more for the whole window, and its first sample already read 599.6 MiB
against 566.7 MiB for base. In series 2 the same binary had started at 567 MiB.

## Memory: freed heap, not live data

Start-up trials, with no traffic, read the daemon 60 seconds after its socket opens. Each row is
one build and one way of starting it.

| Build and start | Starts | RSS MiB | Main heap RSS MiB | Heap in use MiB | RSS after `malloc_trim(0)` MiB |
| --- | ---: | ---: | ---: | ---: | ---: |
| base | 14 | 568.2 to 569.0 | 545.0 | | |
| fixed branch | 14 | 600.8 to 601.3 | 577.7 | | |
| unfixed branch | 8 | 604.7 to 605.2 | 577.7 | | |
| base, trim shim | 3 | 568.4 to 569.0 | 545.0 | 38.1 | 60.2 to 60.8 |
| fixed branch, trim shim | 3 | 600.8 to 601.3 | 577.7 | 38.1 | 59.9 to 60.4 |
| base, shim with a thread | 3 | 568.2 to 568.8 | 545.0 | 38.1 | 60.1 to 60.6 |
| fixed branch, shim with a thread | 3 | 568.1 to 568.6 | 545.0 | 38.1 | 59.9 to 60.4 |

Heap in use is `malloc_info`'s system bytes minus its free bytes. Six of the plain base and fixed
branch starts carried an unused environment variable of 0 to 4,096 bytes. It moved nothing.

- **Both builds hold the same live heap.** About 38.1 MiB is in use after startup, to within
  0.01 MiB, in either memory state of the fixed branch.
- **Most of a quiet daemon's RSS is freed heap.** After startup about 510 MiB of the heap is
  free, and glibc keeps it. `malloc_trim(0)` takes both builds from about 568 or 601 MiB to
  60 MiB.
- **Whether glibc gives back the top 33 MiB depends on heap layout.** In the high state the fixed
  branch's heap reached 583 MiB, 33 MiB past base's 550 MiB, and none of the extra is in use. An
  earlier version of the shim started one idle thread at load, and that alone put the fixed
  branch back at 568 MiB. The same binary started at 567 MiB in both series 2 windows and at
  601 MiB in every plain start launched later. I did not find what differs between those two
  launch contexts.

Across every start of these two builds, base settled at the lower level in 28 of 28 and the
fixed branch in 8 of 26. The counts include the series windows and three starts of each with
`GLIBC_TUNABLES=glibc.malloc.trim_threshold=131072` and the shim with a thread, which matched
the rows without it to within 2 MiB.

### Under jemalloc

The measuring host's live daemon was running with a jemalloc preload. Three starts of each build
with `LD_PRELOAD=libjemalloc.so.2 MALLOC_ARENA_MAX=2`, read as above, alternating:

| Build | Round 1 | Round 2 | Round 3 | Peak RSS MiB |
| --- | ---: | ---: | ---: | ---: |
| base | 129.9 | 125.6 | 127.8 | 644.5 to 757.6 |
| fixed branch | 143.3 | 137.7 | 124.8 | 648.4 to 789.4 |

RSS is the same at 5 and 60 seconds in every start. jemalloc returns most of the freed startup
heap, so a quiet daemon holds about 130 MiB instead of 570 to 600 MiB. The fixed branch read
higher in two of three starts, by 8 to 13 MiB, and lower in the third. Three starts cannot
separate that from noise. A 30-minute jemalloc window pair was started and stopped at 12 minutes
when the mission was restarted, and its samples are not used.

## Series 4: overnight side by side

Base and branch `2ee767c` ran at the same time on one host for 546 minutes, 21:22Z to 07:15Z,
with the Series 2 fixture and six writes a minute. The first 5 minutes are warmup. Each row
covers one hour of the run. The eval runs in this branch ran on their own daemons between
21:50Z and 22:58Z, before the branch diverged.

| Hour | CPU s, base | CPU s, branch | Voluntary switches, base | Voluntary switches, branch | Threads, base / branch | RSS MiB at end, base | RSS MiB at end, branch |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 50.3 | 49.7 | 365,808 | 373,456 | 20 / 21 | 121.7 | 126.7 |
| 2 | 41.5 | 45.0 | 383,166 | 679,244 | 20 / 22 | 144.3 | 153.9 |
| 3 | 41.7 | 44.7 | 396,621 | 652,827 | 20 / 22 | 166.1 | 163.1 |
| 4 | 43.2 | 46.5 | 460,474 | 690,972 | 21 / 22 | 175.8 | 184.5 |
| 5 | 39.8 | 45.5 | 395,671 | 668,904 | 21 / 22 | 179.0 | 194.9 |
| 6 | 40.5 | 44.5 | 409,177 | 594,635 | 21 / 22 | 182.1 | 210.7 |
| 7 | 40.2 | 45.0 | 399,622 | 658,672 | 21 / 22 | 187.7 | 215.6 |
| 8 | 40.9 | 45.6 | 396,359 | 651,155 | 21 / 22 | 200.0 | 229.6 |
| 9 | 42.3 | 46.9 | 446,008 | 685,852 | 21 / 22 | 209.1 | 232.1 |

Involuntary switches were 6,000 to 8,100 an hour on both after the first hour. Across the whole
window, CPU was 0.704 s/min on base and 0.765 on the branch, and all switches were 6,898 and
10,611 a minute. Both started near 600 MiB resident, dropped to about 120 MiB within the first
hour, and then grew steadily.

The divergence is the finding. For the first hour the branch matched base minute by minute at
about 6,100 voluntary switches. From about 23:04Z the branch rose to 7,800, then 9,900, then
10,000 to 12,000 a minute, and gained a 21st and then a 22nd thread. Base never did. Something
on the branch starts about an hour in and then wakes about 70 more times a second. That fits a
timer or a waiting task added by the seat queue, but I have not confirmed it. The probes at the
end matched: the roster read, one seat's work list and mailbox, and a claim write with its
reconcile all cost the same on both builds.

Growth: RSS grew about 88 MiB on base and 105 MiB on the branch over 8.5 hours, still rising
at the end on both. CPU per hour did not grow on either after the first hour.

Evidence: `target/seat-queue-overnight/perf/` in the builder's worktree holds `samples.csv`,
`probe.json`, `memory.txt`, and the daemon and traffic logs for each build.

## Other findings

- **Under glibc, a quiet daemon keeps about 510 MiB of freed heap after startup.** On this
  fixture `malloc_trim(0)` takes either build from about 568 MiB to 60 MiB, and the live heap is
  about 38 MiB. Trimming once after `Store::open` would return it on every start. That is an
  allocator decision for all of st3, and the measuring host's live daemon was already running
  a jemalloc preload to compare retention, so this branch leaves the allocator alone.
- **The roster read is slow on both builds.** `GET /v1/client/agents` took about 540 ms of CPU per
  read on this graph, with no difference between base and branch. It is unrelated to the seat
  queue, and it is worth its own look.
- **A reconcile after one write costs about 43 ms of CPU on base.** The branch adds nothing to
  that after the fix.
- **`st3 agents queue move` can fail with `StaleFence` on a busy graph.** The CLI fences each
  action to the snapshot it just read. Any graph write in between rejects the move, and the CLI
  does not retry. One of the first 273 fixture moves failed this way. Other client-v0 mutations
  in the CLI use the same fence.
- **`st3 agents queue` takes about 4 ms per call from the CLI**, mostly process start. The agent
  queue read is about 0.8 ms of daemon CPU.
- **The existing idle soak sampler counts context switches for the main thread only.**
  `/proc/PID/status` reports one thread. `sampler.py` sums every thread.

## Reproduce

```sh
cargo build --release -p st3 --bin st3        # once per build, copied out between builds
PERF_DATA=$PWD/target/seat-queue-perf \
  scripts/st3-seat-queue-perf/fixture BRANCH_ST3 /tmp/sqp.fix
PERF_DATA=$PWD/target/seat-queue-perf PERF_WRITES_PER_MINUTE=6 \
  scripts/st3-seat-queue-perf/series BASE_ST3 BRANCH_ST3 /tmp/sqp.fix OUT 2 30 5
J="LD_PRELOAD=/usr/lib/x86_64-linux-gnu/libjemalloc.so.2 MALLOC_ARENA_MAX=2"
PERF_DATA=$PWD/target/seat-queue-perf \
  scripts/st3-seat-queue-perf/startup /tmp/sqp.fix OUT 3 "base|BASE_ST3|$J" "branch|BRANCH_ST3|$J"
cc -O2 -shared -fPIC -o trim.so scripts/st3-seat-queue-perf/trim.c
PERF_DATA=$PWD/target/seat-queue-perf PERF_TRIM_SHIM=trim.so \
  scripts/st3-seat-queue-perf/startup /tmp/sqp.fix OUT 3 "base|BASE_ST3|" "branch|BRANCH_ST3|"
ST3_PERF_STORE=/path/to/copy/claims.sqlite3 \
  cargo test --release -p st3 --test seat_queue_perf -- --ignored --nocapture
```

The root holds Unix sockets, so it must be a short path. `PERF_DATA` keeps the large claim stores
on disk instead of under that root.
