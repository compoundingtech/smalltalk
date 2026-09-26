# omp readiness evals — 2026-09-26

On this branch's final st3 source, `0e0358e`, omp passed seat mission work, work wake reliability,
and restart continuity. Cross-harness message wake passed two of three post-fix runs. The third
failed when the model skipped one protocol step, not because of a runtime fault. On the baseline
`9b3c0a3`, omp failed cross-harness message wake by taking another agent's identity, and it needed
three wakes for seat mission work.

These live runs measured omp 18.1.22 seats on `openai-codex/gpt-5.6-luna`. They used four st3
evals: cross-harness message wake, work wake reliability, seat mission work, and restart
continuity. The same evals ran with Codex `gpt-6-luna` and Claude `claude-sonnet-5` in the seat
under test. Each run used its own isolated st3 daemon, state directory, and copied binary. Every
run has a report in its eval's `reports/` directory. The model in each report is counted from that
seat's provider transcript. Every omp assistant turn shows the `openai-codex` provider and the
`gpt-5.6-luna` model.

## Results

The omp fixes are in `93a515a`. The nested-work wake is in `ed38d1f` and was refined in `0e0358e`.
The nested-work wake affects only restart continuity. "After the fixes" means `93a515a` or later.

| Eval | omp on `9b3c0a3` | omp after the fixes | Codex `gpt-6-luna` | Claude `claude-sonnet-5` |
| --- | --- | --- | --- | --- |
| Seat mission work | pass after an eval fix: 3 wakes, claim 54.7 s | pass ×3, one wake each: claims 16.4, 16.9, and 50.3 s | pass: claim 15.3 s | pass: claim 4.0 s |
| Work wake reliability | pass: median claim 8.5 s | pass ×2: median claims 6.4 and 5.8 s | pass: median claim 4.5 s | pass: median claim 2.2 s |
| Cross-harness message wake | fail: one omp seat took the Codex identity | pass ×2; fail ×1 on `0e0358e`: one seat skipped its agreement | pass, both phases | pass, both phases |
| Restart continuity | fail: rc.dev finished its work; the Claude supervisor stalled | pass ×2 on `0e0358e`; three earlier post-fix attempts failed | pass on `ed38d1f` and `0e0358e` | fail on `0e0358e`: 10 extra nested wakes; fail on `ed38d1f`: an extra ledger commit |

Wake-to-claim times run from the durable wake message to `work.claimed`. The 50.3 s seat claim came
from slow model turns while four eval runs shared the provider; it still needed only one wake. The
Codex and Claude columns ran on `9b3c0a3`, except for restart continuity.

## omp failures

1. **The session start used st2 vocabulary.** The pi-family channel opened every omp session with
   "Set your status to available ... Set busy before work". st3 has no status command. omp seats
   searched help output and `.st3/` for availability controls before they claimed work.
2. **One omp seat took another agent's identity.** omp runs its Python `eval` tool with an
   allowlisted environment: basic shell variables plus the `LC_`, `XDG_`, and `PI_` prefixes. That
   environment drops `ST_AGENT`, `ST3_BIN`, and `ST3_ENDPOINT`. In `cross-omp-20260926-a`, one seat
   probed its identity there first and got `None`. It then read the fleet listing, decided that it
   was the Codex seat, and sent `FACT` and `AGREEMENT` messages to the other omp seat as that
   Codex seat. The consensus protocol failed.
3. **Duplicate wakes interrupted the boot turn.** omp's boot turn was already `working` when the
   first wake arrived. omp steered the wake into that turn and recorded delivery at once. The
   reconciler acknowledged only a read, a close, or a later `working` edge, so it sent two more
   wakes 15 seconds apart. Each arrival backgrounded omp's in-flight shell calls. The same race
   added a second wake after omp's cold restart.
4. **Model errors in restart continuity.** In `restart-ompfix-20260926-a`, omp expanded
   `$ST3_MISSION_RUN` instead of `$ST_MISSION_RUN`. It then recorded the pre-restart product under
   `resource/mission-run/mission-run/<run>/pre-restart`, so the step never verified. In
   `restart-ompfix-20260926-b`, omp did items 1 and 2 straight from the parent goal. The nested step
   that asks it to confirm that no item is done then failed.
5. **A protocol slip in cross-harness message wake.** In `cross-ompfix-20260926-c`, one omp seat
   read Codex's fact and agreement, then sent its consensus without ever sending its own agreement.
   Codex waited for the missing agreement, and the controller timed out. The other omp seat sent its
   agreement before its fact. Every message still carried its real sender.
6. **Command-shape retries.** omp ran `conversations ls --as`, `now --as <agent>`,
   `work show --as`, and `work claim` without `--as`. It also tried to claim agentless controller
   steps. The Codex and Claude transcripts show the same kinds of mistake, so the boot contract and
   the `work ls` output cause them; these faults are not specific to omp.

## Fixes on this branch

- `93a515a` replaces the pi-family session context with the st3 boot contract. The context now
  names the seat and says that `$ST_AGENT` and `$ST3_BIN` are in the shell. In all three post-fix
  cross-harness runs, every omp message carried its own sender.
- `93a515a` acknowledges a wake that is delivered while the harness is still `working`. After the
  fix, every omp seat and restart assignment needed one wake.
- `ed38d1f` wakes a ready nested step after its agent submits the parent early. Before this change,
  the parent stayed `verifying` and the idle seat received no further wake. The Claude supervisor
  in restart continuity submitted early in eight of nine runs.
- `0e0358e` holds that nested wake until the harness stops working. A worker that continues in the
  same turn then gets no extra messages or interrupted tool calls. omp's `restart-ompfix-…-e` run
  submitted its parent early and passed because of this change.
- `22158cd` makes seat mission work state the artifact content that its held-out gate checks.
  Before this change, no seat could produce the artifact reliably.
- `3eaf957` repairs the restart-continuity injector and graph judge for the current CLI. Before the
  repair, every restart run stalled at the cold restart.
- `8dd9cbb` excludes st3's `.st3/` boot directory from the restart ledger's clean-worktree check.
- `22158cd` adds `variants/` to the four evals and `WAKE_PAIRS` to cross-harness message wake. The
  cross-harness variants need no Pi installation, which this host does not have.

`cargo test -p st3` passes: 420 library, 89 CLI, 5 client CLI, 21 client contract, 27 example and
eval, and 12 operational-state tests.

## Findings that are not specific to omp

- Parent and nested discipline decides restart continuity. The Claude sonnet worker submitted the
  parent before any nested step in all six of its attempts. The Claude supervisor did so in eight of
  nine runs. Codex luna did it in one of five attempts and omp in four of seven; omp usually
  continued through the nested steps in the same turn.
- The restart coordination gate counts every runtime work message to rc.dev as an assignment. A
  worker that needs a wake per nested step therefore fails the gate, even when all its work is right.
- The Claude supervisor sometimes sends its confirmation twice when it handles the parent before
  its nested steps.
- `work complete` accepts a step whose declared product is missing and leaves the step `verifying`.
  A refusal that named the expected subject would let a model correct a wrong subject.
- An unfiltered `work ls` lists agentless controller and human-gate steps. omp, Codex, and Claude
  all tried to claim them; st3 refused each attempt.
- The local API accepted `--as` and `--from` for another agent from a harness process. Binding a
  harness process to its own identity is a broader trust decision. This branch does not change it.
- A field gate on an exec exit code waits for the step timeout after a non-zero exit. It does not
  fail at once. Cancelled runs record that choice in their reports.

## Not done

- No run used the committed four-harness cross-harness fixture, because Pi is not installed on this
  host. The variants replace the Pi and OMP pair.
- Most configurations ran one to three times. These results are not a statistical measure of
  reliability.
- None of these changes are deployed. The live fleet still runs its installed binary.
