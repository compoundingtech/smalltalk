# Frozen collections-v1 hosted study

This manual Performance dispatch implements only the 2026-10-09 shared assignment
`doc/fleet/smalltalk/speed/2026-10-09/shared-collections-v1-pair-assignment@2d25669da2d9b694cf702f61654abdfb2e11404b8c9ee74ef93b23a7cf6da60a`.
The governor is CI-speed. Work-builder owns the isolated composition, draft #2116,
which must not merge.

Rerun after #2188 (load subscribers wait through a retryable resync), which voided
the first run (38037971326). Baseline is `feb905cddf84113458cec510cd76e22b813ba771` / tree
`13fec0ebc0827b6a5fcb86616a6413f5a0dd57a3`: main with #2188, #2051, #2052 and #2186.
Candidate is `a7d27029ddb20ef6fae8889ceb443d5728011861` / tree
`a0f13d646748196f909602206d32ab23ecffd768`, draft #2116: the same baseline plus #2053
at `ba6c2e9bd` and #2055 at `fb9c38cf2`. The study run is the row-deltas step
`step-run/01a11fa49f4a752084e9a188dd6aba85/row-deltas`.
No per-PR effect is inferred. A SHA correction before
execution requires updating and reviewing these pins, never an edit inside the job.

Dispatch the existing `perf.yml` at the reviewed study control commit with
`collections_study=true` and the exact `study_run` from the tracking mission. It allocates one job on the existing Performance
Namespace 8x16 class, with its existing scheduling priority, a 240-minute bound,
and no ordinary perf-load job in this dispatch. No new profile, admission override,
queue mutation or productive cancellation. Retrying the workflow is refused.
Ordinary PR/main Performance routing is unchanged.

The script freezes clean source trees, verifies lockfiles/Nix/generator/config
parity, and applies one identical report-only patch to each harness. The candidate's
three extra lines starting its two production refreshers are the only allowed
harness difference. Both release perf_load artifacts build serially before any
fixture execution. Release executable hashes, actual sources, arguments, runner/job identity,
logs, exit statuses and patched harnesses are retained. No cached historical
measurements or restored fixture is used.

The only measured order is **B1, C1, C2, B2**. Each binary selects exactly one
nonignored workload test, with scale 1, 120 seconds, collections-v1 and the original
upgrade-under-load and steady-after-migration regimes. The first baseline creates
the standard generated source and peer stores once. Each original regime uses fresh
private workload copies. Saved DB/WAL hashes are frozen after B1 and checked before
and after later executions. Mutation stops the method, without retry. Original
absolute failures remain failures; a natural budget failure does not skip the other
arms. Partial/setup/interrupted results refuse qualification and retain evidence.
No historical baseline is installed or used as this comparison's gate.

Before run creation, publish the finite tracking mission with Speed as `report-to`
and `stalled-after="1h"`. Start it at the reviewed revision before the one dispatch;
keep the existing cut claim. The receipt gate checks only the governor's terminal
file. The study is not a second agent, a fleet census or a production collector.

## Prespecified comparison and CPU limits

For each original regime, pair B1 with C1 and B2 with C2. Retain each collection's
snapshot and connect+snapshot p99 and sample count, subscribers, frames and failures.
Report both replicate values, paired deltas, arithmetic two-replicate means and
min/max spread; no best-run choice or ratios of unrelated historical tails.
Requested missions/work p99 is strictly **less than 300 ms**, and a met goal also
requires all 22 samples on both snapshot paths, 22 correct subscribers and no
reported request failures in both candidate replicates. Zero/partial samples keep
their raw values and negative correctness; they cannot meet the requested goal.
Raw CPU output includes both paired observations and per-arm means and spread.
Original reconcile/invalidation path rows and legacy charges are descriptive context. Raw daemon cores must
be **no worse than their paired baseline**, without the repository's 2x/slack rule.
All six additional collections and the agents roster retain their own raw outcomes.
These requested observations do not waive any original correctness or absolute gate.

The observation patch retains the existing raw daemon CPU formula and denominator.
It records process user+system CPU seconds, excluded load/peer thread CPU seconds,
monotonic sampling brackets, workload start/stop and drain end. CPU reads are
sequential, and the inherited denominator starts after the initial CPU samples.
The patch exposes those alignment limits; it does not silently redefine the metric
or assert identical instantaneous endpoints.

The fixture owner supplies no qualified reconcile span accounting in this slice.
Legacy request charges are thread CPU milliseconds from top-20 rolling-300-second,
drop-charged spans: boundary-crossing spans and writer propagation are unqualified.
The report retains those charges descriptively, never subtracts them, and labels
adjusted cores, every reconcile mode and within-mode comparison **UNKNOWN**.
Thus combined CPU qualification remains UNKNOWN even when raw observed comparisons
improve. No threshold chosen from the four observations classifies favorable modes;
there is no extra execution to obtain matching modes. No pacing or production
performance-collector changes are made.

An execution result contains all budget failures and incomplete data. Job success
means four structurally complete executions also passed their original absolute
checks; it never means deployed/fleet/day acceptance or qualified adjusted CPU.
The complete result and all logs are uploaded even after a failure. Generated input
identities/schema recipe and release executable identities are retained as hashes,
without uploading the generated stores or the two release executables.
