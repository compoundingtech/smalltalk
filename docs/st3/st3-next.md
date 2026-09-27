# st3-next: seat queues and omp readiness on st3

`agent/st3-next` is what `st3` becomes if it takes both `agent/seat-queue` and `agent/omp-ready`.
It starts at `st3` `9b3c0a3` and merges `agent/seat-queue` at `bdf4bb2` and `agent/omp-ready` at
`b6d172e`. The merged code passes the st3 test suites, adds no new lint, and passed the seat queue
eval and the omp versions of cross-harness message wake and work wake reliability, one run each.

Fast-forward `st3` to `agent/st3-next` to take both branches.
[Fast-forward steps](#fast-forward-steps) has the exact commands. Nothing is deployed by the
fast-forward.

## What the branch holds

| Commit | Change |
| --- | --- |
| `a47f55d` | Merges `agent/seat-queue`. `st3` had not moved since that branch's last merge from it, so the tree equals `bdf4bb2`. |
| `a058499` | Merges `agent/omp-ready`. Two files conflicted; [Merge resolution](#merge-resolution) explains them. |
| `2a96d51` | Adds a seat queue test and one paragraph in [Agent seat queues](seat-queue.md) for the rule the two branches produce together. |

Later commits add only this document and the three eval reports.

- **Seat queues** (`agent/seat-queue`) give each agent seat one ordered queue of mission runs. A
  person, or an agent with `queue-authority` for the seat, can move a run with
  `st3 agents queue move`. `work claim` refuses a later run's step while an earlier run has ready
  work for the seat. [Agent seat queues](seat-queue.md) explains the model and its tests.
- **omp readiness** (`agent/omp-ready`) fixes the omp failures that live evals exposed. A harness
  process can no longer act as another agent. A wake delivered into a working turn counts as
  acknowledged. A parent submitted before its nested step no longer stalls that step. An idle worker
  learns which declared product its step waits for. [Running st3 with omp](omp.md) covers setup and
  the model choice.

No `Cargo.toml` or `Cargo.lock` changed, so the Nix vendoring hash is unaffected.

## Merge resolution

`agent/seat-queue` moved the seat's next-work choice out of the reconciler into
`seat_queue::select`. The reconciler wake, `st3 agents show`, the queue view, and the `work claim`
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

All commands ran in a scrubbed environment: `env -i`, a scratch `HOME`, and no live st3 endpoint,
PTY registry, or hooks.

| Check | Commit | Result |
| --- | --- | --- |
| `cargo fmt --all -- --check` | `2a96d51` | pass |
| `git diff --check` | `2a96d51` | pass |
| `cargo test -p st3` | `2a96d51` | pass: 438 library, 94 CLI, 6 client CLI, 22 client contract, 28 example and eval, 12 operational-state; the seat queue performance test is ignored by design |
| `cargo test --workspace --no-fail-fast` | `a058499` | 2,359 passed, 16 ignored, 12 failed; every failure is an environment gate in unchanged code (below) |
| `cargo clippy --workspace --all-targets -- -D warnings` | `a058499` | fails on existing lints in `st2`, `stui`, and `st-runtime`, none of which changed |
| `cargo clippy -p st3 --all-targets` | `2a96d51` | five warnings, each on a line already in `st3` `9b3c0a3`; none new |

The 12 workspace failures are all in `st2` and `st2-resource-providers`, whose sources are identical
to `st3` `9b3c0a3`. Each test stops at a precondition that the scrubbed shell does not meet:

- 3 need a systemd user manager (`nomad_survival`, `transport_isolation`). They are native gates;
  the Nix sandbox has no user manager either.
- 2 need the OTLP collector binary that the Nix check pins (`otel_export`).
- 6 need the GitHub issue and pull request components that the Nix provider check builds.
- 1 needs a live PTY statistics source (`pty_stats`).

The Nix flake checks did not run locally. They run in CI on the draft pull request into `main`.

## Evals

Each run used its own isolated st3 daemon, state directory, and copies of the `st3` and `st2`
binaries built from `a058499`. The eval files came from the same commit. Every judge is mechanical.

| Eval | Seats | Run | Result | Duration |
| --- | --- | --- | --- | ---: |
| Seat queue | Claude `claude-sonnet-5` seat and a model-free chief | `6bd0949c` | pass | 41.4 s |
| Cross-harness message wake | two omp seats on `openai-codex/gpt-6-astra`, paired with fixed Codex `gpt-6-sol` and Claude `opus` seats | `cross-omp-astra-next-20260927-a` | pass | 180.4 s |
| Work wake reliability | omp worker on `openai-codex/gpt-5.6-luna` | `wake-omp-next-20260927-a` | pass | 154.7 s |

- **Seat queue.** The chief moved charlie before bravo while the seat held alpha's first step. The
  seat then took alpha draft, charlie, alpha publish, and bravo, each on its first wake, which is
  queue order. No held claim was preempted. The seat read the controller's step but did not claim
  it.
- **Cross-harness message wake.** Both phases completed with 8 kickoffs and exactly 24 protocol
  messages, each sent by its real seat. Incoming mail backgrounded 7 omp tool calls, 6 of them
  sends; no send was repeated. No omp seat claimed the controller's step. The startup phase took
  102.9 s because the fixed Codex seat was slow: it paused 29 s after reading its boot file and sent
  its fact 63 s after the kickoff. The Claude and omp pair finished that phase in 42 s. The idle
  phase took 39.9 s.
- **Work wake reliability.** Six assigned steps across fresh runs, two live revisions, and a
  replacement incarnation each needed one wake. Wake to claim was 3.6 s median and 6.8 s maximum.

The cross-harness run uses `gpt-6-astra` because [Model choice](omp.md#model-choice) recommends it
for omp seats. Work wake reliability has only the `gpt-5.6-luna` omp variant. Each eval ran once, so
these results show that the merged code works end to end, not how reliable it is. The per-run
reports hold the timelines, token counts, and transcript observations:

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

   The last command lists the commits that add this document and the eval reports, `2a96d51`,
   `a058499`, and `a47f55d`.

2. Fast-forward and publish:

   ```sh
   git switch st3
   git merge --ff-only origin/agent/st3-next
   git push origin st3
   git push github-public st3
   ```

   `github-public` is the GitHub remote; use that remote's name in your clone.

If `st3` has moved since `9b3c0a3`, do not force it. Merge the new `st3` into `agent/st3-next`,
run `cargo test -p st3` again, and fast-forward from there.

After the fast-forward:

- `agent/seat-queue` and `agent/omp-ready` are fully contained in `st3` and can be retired.
- The pull request branch from `st3` into `main` was built on `st3` `9b3c0a3`. Merge the new `st3`
  into it so `main`'s checks cover both features.
- Deploy separately. Installing a build from the new `st3` restarts the daemon, which starts a new
  series for any running idle soak. Keep `st` pointing at the same `st3` build.
