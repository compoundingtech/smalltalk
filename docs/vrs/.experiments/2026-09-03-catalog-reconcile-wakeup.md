# Catalog reconcile wakeup investigation

Date: 2026-09-03

## Question

Why did three successful `axe agent new` publications wait 17–21 seconds for the resident st2 supervisor, and which wake contract removes that latency without weakening direct Agent Spec authoring?

## Method

1. Read issue #430 and split each canary duration at the published Agent Spec mtime.
2. Inspected `src/main.rs`, `src/run.rs`, `src/watch.rs`, `src/agent_publish.rs`, and the catalog generation fence.
3. Inspected `src/run.rs` at the exact reported revision `fbd1ff6` rather than inferring behavior from the current branch.
4. Traced the dotfiles `axe agent new` caller through bundle publication and its PTY wait loop.
5. Added a Linux watcher regression that atomically renames a complete Agent bundle into a catalog containing 750 existing agents.
6. Added a deterministic resident-loop regression that creates a new direct Agent Spec after the first pass and requires its launch before a 60-second fallback interval.
7. Prototyped a second watcher instance that accepts only atomic replacement of `.st2/catalog-generation`.

## Evidence

- Issue #430 records three supervisor-side waits of 21.0, 18.4, and 17.4 seconds against a 30-second interval.
- `git show fbd1ff6:src/run.rs` contains `best_effort_catalog_watcher`, `wait_for_reconcile`, and the mutation channel used by `up_loop`. The `timer-only; no fs-watch` comment cited by the issue belongs to `up_loop_specs`, the static single-spec mode, not the catalog mode used by the dev3 service.
- The dev3 service command was `st2 up --catalog ... --host dev3`, so it selected the catalog loop rather than static-spec mode.
- The pre-change atomic bundle regression passed against the existing declaration watcher. The atomic rename shape alone does not reproduce the production miss.
- Source tracing shows `axe agent new` invokes the pinned `st2 agent publish --bundle ...` and then polls `pty list` every 200 ms. Axe sends no wake request.
- Every successful built-in declaration transaction advances `catalog-generation` after durable publication while it still owns the exclusive catalog lock.
- The production catalog contained approximately 750 agents. Its full tree contained 20,579 directories, although declaration watcher pruning excludes runtime and Resource payload descendants. Retained supervisor logs showed no watcher setup warning. These readings do not prove watcher exhaustion or overflow.

## Result

The issue's timer-only source diagnosis is falsified. The observed latency is real, but the exact production declaration-event loss is not reproduced by the supported atomic bundle publication shape in an isolated catalog. Watcher setup failure, partial subscription failure, queue overflow, and lost backend events remain possible failure classes; available evidence does not select one.

The selected solution does not disguise that uncertainty. A dedicated constant-cost catalog-generation watcher makes transactional publication latency independent of the declaration watcher. The existing declaration watcher remains for authorized direct atomic KDL edits. Both queue work into the same serialized supervisor loop, and the timer remains the recovery path.

The Linux wakeup regression set passed 25 tests, including the 750-agent atomic
bundle case, generation replacement and reinstallation after control-directory
replacement, declaration subscription refresh, disconnected-channel fallback,
and no unintended wakeups.
The deterministic resident-loop proof launched the new declaration in its
second pass with a 60-second timer, and the idle-supervisor proof completed
without a spin.

## Conclusion

The experiment selects independent generation and declaration wake channels.
The source-level change can guarantee prompt transactional publication without
pretending to have identified the lost production event. A post-deployment
canary remains useful fleet evidence, but it is not part of the source-level
mechanism.

## VRS Impact

Root requirement R40 now requires prompt catalog convergence for cooperative
transactions and authorized direct atomic publication. The root specification
defines the two watcher instances, accepted paths, serialized coalescing, and
timer fallback. Decision 0015-catalog-commits-and-direct-edits-use-independent-wake-channels records the rejected public-request and
single-channel alternatives.
