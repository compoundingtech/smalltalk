# st3-next: seat queues and omp readiness on st

`agent/st3-next` is what `st3` becomes if it takes both `agent/seat-queue` and `agent/omp-ready`.
It starts at `st3` `9b3c0a3` and merges `agent/seat-queue` at `5fa3487` and `agent/omp-ready` at
`b6d172e`, the final heads of both branches. The merged code passes the st test suites, adds no
new lint, and passed the seat queue eval twice and the omp versions of cross-harness message wake
and work wake reliability once each.

Fast-forward `st3` to `agent/st3-next` to take both branches. The seat queue branch's own verdict
is "not yet" because of an idle wakeup cost that
[Choosing what st takes](#choosing-what-st-takes) weighs; the recommendation is to take both and
fix that cost on `st3`. [Fast-forward steps](#fast-forward-steps) has the exact commands for both
choices. Nothing is deployed by the fast-forward.

## Choosing what st takes

The seat queue branch ran a 9-hour side-by-side idle run of base and branch overnight. It found no
correctness problem. After about an hour the branch daemon woke about 70 more times a second than
base: 10,000 to 12,000 voluntary context switches a minute against about 6,500. It held 22
threads against base's 21, and it used 0.765 against 0.704 s of CPU a minute, about 4 seconds more
an hour. The extra wakeups stopped growing after the second hour. Resident memory grew 11 to 13 MiB
an hour on both builds. The branch's builder has not found the cause, and the measured build,
`2ee767c`, predates the two commits that add agent queue moves and the seat declaration check.
[Seat queue performance](seat-queue-performance.md#verdict) has the numbers.

- **Take both (recommended).** Fast-forward `st3` to `agent/st3-next` and find the wakeup source on
  `st3`. The cost is small and bounded. The feature works: 16 of 18 live seat queue runs on
  Claude, Codex, and omp seats passed, one failed before any work existed, one was void and led
  to the fix in `16aa754`, and both runs on this branch passed.
- **Take omp readiness only.** Fast-forward `st3` to `agent/omp-ready`. It starts at `st3`
  `9b3c0a3`, so this is also a fast-forward, and it carries none of the seat queue. Seat queues
  then wait until the wakeup source is found.

## What the branch holds

| Commit | Change |
| --- | --- |
| `a47f55d` | Merges `agent/seat-queue`. `st3` had not moved since that branch's last merge from it, so the tree equals `bdf4bb2`. |
| `a058499` | Merges `agent/omp-ready`. Two files conflicted; [Merge resolution](#merge-resolution) explains them. |
| `2a96d51` | Adds a seat queue test and one paragraph in [Agent seat queues](seat-queue.md) for the rule the two branches produce together. |
| `2549baa` | Merges `agent/seat-queue` again at `5fa3487`. Its three new commits add only the overnight performance results and eleven eval reports; no code changed. `agent/omp-ready` had no new commits. |

The other commits add only this document and eval reports, so the head's code equals `2a96d51`'s.

- **Seat queues** (`agent/seat-queue`) give each agent seat one ordered queue of mission runs. A
  person, or an agent with `queue-authority` for the seat, can move a run with
  `st agents queue move`. `work claim` refuses a later run's step while an earlier run has ready
  work for the seat. [Agent seat queues](seat-queue.md) explains the model and its tests.
- **omp readiness** (`agent/omp-ready`) fixes the omp failures that live evals exposed. A harness
  process can no longer act as another agent. A wake delivered into a working turn counts as
  acknowledged. A parent submitted before its nested step no longer stalls that step. An idle worker
  learns which declared product its step waits for. [Running st with omp](omp.md) covers setup and
  the model choice.

No `Cargo.toml` or `Cargo.lock` changed, so the Nix vendoring hash is unaffected.

## Merge resolution

`agent/seat-queue` moved the seat's next-work choice out of the reconciler into
`seat_queue::select`. The reconciler wake, `st agents show`, the queue view, and the `work claim`
order check all use that one selector. `agent/omp-ready` changed the reconciler's old inline copy
of the same choice:

- a parent that its agent submitted while one of its own nested steps is still ready no longer
  occupies the seat;
- that nested step is no longer reached through the submitted parent, so it gets its own wake.

`a058499` moves both rules into `seat_queue::select`, so every view agrees with the wake. The
reconciler keeps omp-ready's deferral: the nested wake waits until the turn that submitted the
parent has ended. It uses the same nesting test as the selector. The runtime document now
describes the seat queue together with omp-ready's acknowledgement rules.

Together the branches produce one rule that neither had alone. A nested step under a submitted
parent is ordered as its run's ready work. If an earlier run has ready work for the seat, that run
is woken first, and `work claim` refuses the nested step until then. A submitted parent carries no
lease, so the seat can claim the earlier run's step. `2a96d51` pins this with
`a_submitted_parent_frees_the_seat_and_its_nested_step_keeps_queue_order`.

## Checks

All commands ran on `2549baa`, the final merge, in a scrubbed environment: `env -i`, a scratch
`HOME`, and no live st endpoint, PTY registry, or hooks. The same checks on `a058499` and
`2a96d51` gave the same results.

| Check | Result |
| --- | --- |
| `cargo fmt --all -- --check` | pass |
| `git diff --check` | pass |
| `cargo test -p st3` | pass: 438 library, 94 CLI, 6 client CLI, 22 client contract, 28 example and eval, 12 operational-state; the seat queue performance test is ignored by design |
| `cargo test --workspace --no-fail-fast` | 2,360 passed, 16 ignored, 12 failed; every failure is in code identical to `st3` `9b3c0a3` (below) |
| `cargo clippy --workspace --all-targets -- -D warnings` | fails on an existing lint in `st-runtime`, which did not change, and stops there; the `a058499` run also listed existing lints in `st2` and `stui` |
| `cargo clippy -p st3 --all-targets` | five warnings, each on a line already in `st3` `9b3c0a3`; none new |

The 12 workspace failures are in `st2`, `st2-resource-providers`, and `st-runtime`, whose sources
are identical to `st3` `9b3c0a3`. Eleven stop at a precondition that the scrubbed shell does not
meet:

- 3 need a systemd user manager (`nomad_survival`, `transport_isolation`). They are native gates;
  the Nix sandbox has no user manager either.
- 2 need the OTLP collector binary that the Nix check pins (`otel_export`).
- 6 need the GitHub issue and pull request components that the Nix provider check builds.

The twelfth, `st-runtime`'s `terminal_signal_targets_the_terminal_process_group_not_the_daemon`,
waits 2 seconds for a signalled process group to exit. It failed once in this run, passed alone in
0.02 seconds, and passed in the `a058499` run. The `pty_stats` failure from the `a058499` run did
not recur.

The Nix flake checks did not run locally. They run in CI on the draft pull request into `main`.

## Evals

Each run used its own isolated st daemon, state directory, and copies of the `st3` and `st2`
binaries built from the commit in the table. The eval files came from the same commit. Every judge
is mechanical.

| Eval | Seats | Commit | Run | Result | Duration |
| --- | --- | --- | --- | --- | ---: |
| Seat queue | Claude `claude-sonnet-5` seat and a model-free chief | `2549baa` | `598f86e7` | pass | 88.9 s |
| Seat queue | Claude `claude-sonnet-5` seat and a model-free chief | `a058499` | `6bd0949c` | pass | 41.4 s |
| Cross-harness message wake | two omp seats on `openai-codex/gpt-6-astra`, paired with fixed Codex `gpt-6-sol` and Claude `opus` seats | `a058499` | `cross-omp-astra-next-20260927-a` | pass | 180.4 s |
| Work wake reliability | omp worker on `openai-codex/gpt-5.6-luna` | `a058499` | `wake-omp-next-20260927-a` | pass | 154.7 s |

- **Seat queue.** In both runs the chief moved charlie before bravo while the seat held alpha's
  first step. The seat then took alpha draft, charlie, alpha publish, and bravo, each on its first
  wake, which is queue order. No held claim was preempted, and the seat did not claim the
  controller's step. In the final run the seat also tried to claim bravo before its wake, and
  `work claim` refused it and named alpha publish, the step ahead of it in the queue. That run
  shared the host with this branch's test builds, which likely explains its longer duration.
- **Cross-harness message wake.** Both phases completed with 8 kickoffs and exactly 24 protocol
  messages, each sent by its real seat. Incoming mail backgrounded 7 omp tool calls, 6 of them
  sends; no send was repeated. No omp seat claimed the controller's step. The startup phase took
  102.9 s because the fixed Codex seat was slow: it paused 29 s after reading its boot file and sent
  its fact 63 s after the kickoff. The Claude and omp pair finished that phase in 42 s. The idle
  phase took 39.9 s.
- **Work wake reliability.** Six assigned steps across fresh runs, two live revisions, and a
  replacement incarnation each needed one wake. Wake to claim was 3.6 s median and 6.8 s maximum.

The cross-harness and work wake runs used `a058499`, which differs from the head only by one unit
test. The cross-harness run uses `gpt-6-astra` because [Model choice](omp.md#model-choice)
recommends it for omp seats. Work wake reliability has only the `gpt-5.6-luna` omp variant. Each
eval ran once or twice, so these results show that the merged code works end to end, not how
reliable it is. The per-run reports hold the timelines, token counts, and transcript observations:

- [seat queue `598f86e7`](../../evals/st3/seat-queue/reports/2026-09-27-598f86e7.md)
- [seat queue `6bd0949c`](../../evals/st3/seat-queue/reports/2026-09-27-6bd0949c.md)
- [cross-harness message wake](../../evals/st3/cross-harness-message-wake/reports/2026-09-27-cross-omp-astra-next-20260927-a.md)
- [work wake reliability](../../evals/st3/work-wake-reliability/reports/2026-09-27-wake-omp-next-20260927-a.md)

## Fast-forward steps

Run these in the checkout that has `st3` checked out, with a clean working tree.

1. Fetch the branch and confirm that `st3` has not moved:

   ```sh
   git fetch origin st3 agent/st3-next
   git rev-parse st3 origin/st3            # both 9b3c0a354bd7140790dbffcc4b1d23210fc010d9
   git merge-base --is-ancestor st3 origin/agent/st3-next && echo fast-forward
   git log --oneline --first-parent st3..origin/agent/st3-next
   ```

   The last command lists the commit that adds this document's final results, `2549baa`,
   `389eb50`, `2a96d51`, `a058499`, and `a47f55d`.

2. Fast-forward and publish:

   ```sh
   git switch st3
   git merge --ff-only origin/agent/st3-next
   git push origin st3
   git push github-public st3
   ```

   `github-public` is the GitHub remote; use that remote's name in your clone.

To take omp readiness only, fetch `agent/omp-ready` in step 1, confirm
`git merge-base --is-ancestor st3 origin/agent/omp-ready`, and fast-forward to
`origin/agent/omp-ready` at `b6d172e` in step 2 instead. `agent/seat-queue` then stays open.

If `st3` has moved since `9b3c0a3`, do not force it. Merge the new `st3` into `agent/st3-next`,
run `cargo test -p st3` again, and fast-forward from there.

After the fast-forward:

- `agent/seat-queue` and `agent/omp-ready` are fully contained in `st3` and can be retired.
- The pull request branch from `st3` into `main` was built on `st3` `9b3c0a3`. Merge the new `st3`
  into it so `main`'s checks cover both features.
- Deploy separately. Installing a build from the new `st3` restarts the daemon, which starts a new
  series for any running idle soak. Keep `st` pointing at the same `st3` build.
