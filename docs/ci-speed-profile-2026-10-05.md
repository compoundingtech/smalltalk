# Workspace CI profile for October 5 2026

Workspace CI spends most of its critical path executing tests, followed by building
their executables. Merge groups also have a substantial runner queue tail. The next
change should partition test work using existing capacity, protect the reserved
runners from ordinary work, and reuse compiled executables. Increasing the number
of processes on ci1 alone has no demonstrated benefit.

This is a baseline and a proposal for the implementation step. Savings below are
estimates; no CI change or measured improvement is claimed here.

## Sample and measurement

The sample contains all successful Workspace CI runs in the latest 100 workflow
runs retrieved at approximately 08:22 UTC whose creation time is October 5,
02:37:00 through 07:41:59 UTC: 32 PR runs and 13 merge-group runs. Job timestamps
and logs come from the GitHub Actions API. Percentiles use nearest rank. Workflow
elapsed time is `updated_at - created_at`; job queue time is
`started_at - created_at`. Time spent awaiting admission to the merge queue before
the workflow event is outside this measurement.

| Cohort | Runs | Workflow median | Workflow p90 | Linux tests median | Test queue median | Test queue p90 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| All PRs | 32 | 17m 05s | 18m 55s | 16m 32s | 10s | 33s |
| PRs on ci1 | 11 | 15m 33s | 17m 00s | 14m 12s | 1s | 2m 00s |
| PRs on Namespace | 21 | 17m 41s | 20m 07s | 16m 58s | 10s | 12s |
| Merge groups, all on ci1 | 13 | 19m 12s | 30m 32s | 16m 37s | 1m 36s | 12m 16s |

This sample extends the mission's earlier observation: the additional merge
groups near 07:29 and 07:31 introduce large queue waits. The maximum measured
test queue wait is 19m 06s. The workflow medians combine execution, scheduling,
and final checks; separately calculated stage medians do not add up to them.

## Typical successful runs

The [PR run](https://github.com/compoundingtech/smalltalk/actions/runs/37273641006)
checked commit `dc61639ed03d95eaeb6e4c6c280537747e58ed8c`.
The [merge-group run](https://github.com/compoundingtech/smalltalk/actions/runs/37264147339)
checked commit `5942ce46ce55d8c187ed58896999f19de41f13da`.
Both test jobs waited one second after becoming runnable.

| Serial Linux test stage | PR | Merge group |
| --- | ---: | ---: |
| Historical messaging binary | 2s | 2s |
| Provider component fixtures | 14s | under 1s |
| Matching rendered hooks, including Cargo build | 27s | 27s |
| Selected test targets, build only | 202s | 156s |
| Explicit mail canaries, including list and run setup | 66s | 67s |
| Standalone conversation model and workspace test setup | about 5s | about 4s |
| Workspace nextest execution | 658.414s | 676.042s |
| st2 retained targets, execution only | 9.921s | 8.937s |
| Entire Run nextest step | 675s | 689s |
| Entire Linux test job | 1000s | 955s |
| Entire workflow | 1020s | 991s |

The PR workspace run executed 3,730 tests across 41 binaries, with 90 skipped by
the existing selection, including 78 via the CI default filter. The st2 invocation
executed another 339 tests across three binaries. The standalone conversation
model executed 10 unit tests and its empty doctest suite. The explicit canary
invocation executed 17 tests with zero retries. The merge run had 3,723 workspace
tests and 11 explicit canaries. These different counts reflect different sources;
compare the selected inventory at each exact revision when validating changes.

Both workspace runs had two flaky tests. Nextest permits two retries with a fixed
30-second delay, except for the existing overrides that require immediate
failure. The logs do not provide every passing test's duration, so neither shard
balance nor total retry overhead can be calculated precisely from this baseline.
The selected canaries also belong to the full workspace run; running them twice
adds work. Their explicit zero-retry gate must survive any deduplication.

Other jobs overlap the Linux test job. In the PR run, Clippy plus code generation
took 148s, fleet compatibility took 144s, and the cost stage took 389s. Isolation
compiled its integration archive in 25s, ran three scope tests in 24s, compiled
the gateway binary in 127s, and ran its gateway test in 43s. Generated-file checks
took 17s; TypeScript contracts and consumers took 8s combined. In the merge run,
Clippy plus code generation took 118s, fleet compatibility 133s, scope tests 22s,
gateway build 87s, and gateway test 38s. The cost job was skipped by that workflow's
existing rules. The final mail and Linux gate checks took only a few seconds.

## CPU scheduling and cache evidence

At 08:23:49 UTC, ci1 had 24 logical CPUs and load averages 46.76, 39.64, and
39.73. Four one-second `vmstat` intervals showed 79–86% user plus system CPU,
3–5% I/O wait, and 15–29 runnable processes. Concurrent processes included rustc,
test executables, and fixture daemons. A second sample at 08:28:00 showed 13–17%
user plus system CPU and 14–15% I/O wait, with load still 37.22. Contention varies
during a run; neither short sample measures the historical runs' average CPU.

All six runner services have unlimited CPU quotas and no CPU affinity. Their
environment sets four Cargo build jobs and 16 local test threads per job. Multiple
test jobs can therefore request far more concurrency than the host supplies.
The simultaneous Nix release command uses four cores and one build job. The
mission requires keeping the four general runners and the priority and merge
reservations; additional runner registrations would not supply additional CPUs.

The observed reservation is porous: the PR's Clippy job used `ci1-merge-1`.
Earlier merge jobs also used general runners. Current workflow selection gives
the same selected pool to multiple supporting jobs, so assigning a whole workflow
to a single reserved runner can serialize those jobs behind the long test job.
The organization runner-label API was unavailable with the current token, so the
exact deployed label assignments still need inspection during implementation.

On ci1, the remote Cargo, Nix, and exact-source snapshot restore steps were
**skipped**, because host-local caches are enabled. This is not evidence of a
100% hit rate. The PR still compiled provider fixtures and workspace crates.
Its hooks build took 20.73s inside the 27s step; workspace compilation took
3m 11s plus a second 4.98s build. Fleet, cost, and tests logged compilation of
the same workspace crates in separate checkouts.

A later live sccache sample across five active runner ports recorded 75 hits and
31 misses, about 71% hits among completed cacheable requests. There were 209
non-cacheable calls among 316 requests; these are not all failed cache lookups,
and the counters belong to the current server lifetimes, not the historical
sample. No cache timeouts, read errors, or write errors were reported. Binary
builds and linking still take time even with cached library compilation. A
snapshot HIT logged inside the PR's parallel Clippy stage did not eliminate its
2m 18s Cargo work.

Namespace PRs spent a median 32.5s restoring Cargo, 5s restoring Nix, 57.5s
installing hooks, 192s building selected tests, and 30s packing and uploading
snapshots. The matching ci1 PR test-build median was 128s. Host-local warmth
helps compilation, but tests and shared host scheduling remain material costs.

An [October 3 merge run](https://github.com/compoundingtech/smalltalk/actions/runs/37162972734)
finished in 8m 54s, with its Linux test job taking 8m 43s. Its selected-test build
took 106s and workspace execution 351.036s, versus 156–202s and 658–676s in
the two detailed October 5 runs. It ran 3,413 workspace tests and lacked the
separate canary stage. This demonstrates a faster historical execution, but
suite growth and workflow changes prevent attributing the entire difference to
CPU contention.

## Three proposed cuts

| Priority | Change | Expected savings and basis |
| --- | --- | --- |
| 1 | Partition the main workspace tests into two balanced execution shards using existing independent capacity. Build once and distribute the executables. Preserve all selected tests, special feature checks, and the explicit canary result. | **4–5 minutes** off the detailed 11-minute workspace execution, if the larger shard finishes in 6–7 minutes. An ideal equal split saves about 5.5 minutes; balance, archive transfer, and real spare CPU determine the attainable result. Splitting onto the same saturated host at the same total concurrency offers no guaranteed saving. |
| 2 | Protect priority and merge labels from ordinary jobs; give supporting jobs a general or existing fallback pool. Admit heavy jobs according to host capacity and bound their combined build/test concurrency. | **About 1.5 minutes** of median merge test queue wait is available to recover, with **up to 12 minutes at p90** in this sample. Recovering **1–3 minutes of execution** is a hypothesis supported by contention and the historical comparison, requiring a controlled measurement. Queue and execution benefits depend on other admitted work and must not be added blindly. |
| 3 | Reuse an exact-source test archive across shards and compatibility consumers; keep library and Nix caches warm while avoiding repeat executable builds. Preserve compiler, flags, platform, source, feature, and fixture provenance. | **1–2.5 minutes** from the measured 2.6–3.4-minute test build plus the 27-second hooks stage is a target when compatible outputs are reusable. New sources and incompatible features still require compilation. Secondary reduction of canary duplication could save part of another 65 seconds while retaining zero retries. |

The estimates overlap. A reasonable first target is a warm successful PR and
merge-group critical path below 10 minutes, followed by measured tail reduction.
Use the same source and selected inventory for before/after runs. Record queue,
compilation, archive/cache hit and transfer time, each test invocation, per-test
durations, retries, CPU and I/O samples, and the final required checks. Require
the union of shard inventories to equal the current selected suite, including
the explicit zero-retry canaries and standalone feature checks. Use existing
priority and merge reservations throughout.

Raw API responses, exact-revision scripts, logs, downloaded test artifacts,
analysis, and host samples are retained privately under
`~/.local/state/st3/ci-speed-profile-2026-10-05/`. Public links above allow checking
the representative timings without publishing machine-local environment data.
