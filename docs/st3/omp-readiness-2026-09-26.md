# omp readiness evals — 2026-09-26

On this branch's final st3 behavior (`55c777b`, restored by `46fe831`), omp passes seat mission work
and work wake reliability on every run. It passes restart continuity in two of three runs and
cross-harness message wake in three of six. The remaining omp failures are model protocol slips,
plus one duplicate send when a steered message backgrounded a tool call. Codex fails
cross-harness message wake at a similar rate, for a different reason.

On the baseline `9b3c0a3`, omp failed cross-harness message wake by speaking as another agent. It
needed three wakes to claim seat mission work. Restart continuity could not pass for any harness
until its eval and a runtime wake gap were repaired.

These live runs measured omp 18.1.22 seats on `openai-codex/gpt-5.6-luna`. They used four st3
evals: cross-harness message wake, work wake reliability, seat mission work, and restart
continuity. The same evals ran with Codex `gpt-6-luna` and Claude `claude-sonnet-5` in the seat
under test. Each run used its own isolated st3 daemon, state directory, and copied binary. All 113
runs that created a mission run have a report in their eval's `reports/` directory, dated by the
run's start in UTC. The model in each report is counted from
that seat's provider transcript. Every omp assistant turn shows the `openai-codex` provider and the
`gpt-5.6-luna` model.

## Final behavior

The table counts runs whose seat code path matches the final source. omp counts include
`21a507e`, which differs only in one refusal message. Codex and Claude counts include `21a507e`,
`0c7a764`, and `c8288a8`, whose Codex and Claude paths are identical to the final source.

| Eval | omp `gpt-5.6-luna` | Codex `gpt-6-luna` | Claude `claude-sonnet-5` |
| --- | --- | --- | --- |
| Seat mission work | 2 of 2 pass; one wake, claims 12.2 and 12.6 s | 1 of 1 pass; claim 11.1 s | 1 of 1 pass; claim 3.8 s |
| Work wake reliability | 2 of 2 pass; median claims 9.5 and 5.6 s | 1 of 1 pass, median 4.6 s; 1 eval fault | 1 of 1 pass; median 2.5 s |
| Cross-harness message wake | 3 of 6 pass | 2 of 3 pass | 3 of 3 pass |
| Restart continuity | 2 of 3 pass | 1 of 1 pass | 0 of 1 pass |

Wake-to-claim times run from the durable wake message to `work.claimed`. Cross-harness message
wake pairs fixed Codex `gpt-6-sol` and Claude `opus` seats with two seats of the harness under test.
Restart continuity keeps rc.sup on Claude `claude-sonnet-5` and changes rc.dev.

Failures behind those counts:

- **omp cross-harness message wake.** One seat overlooked an agreement steered into its running
  turn and never sent its consensus (`cross-omp-end-20260926-b`).
  One seat sent a fact twice after a steered message backgrounded its send
  (`cross-omp-head-20260926-c`). In one run the fixed Codex partner stopped
  (`cross-omp-head-20260926-b`).
- **omp restart continuity.** One run failed only the coordination gate: rc.dev submitted a parent
  before its nested steps and ended the turn, and the Claude supervisor sent an untagged
  confirmation (`restart-omp-end-20260926-b`).
- **Codex cross-harness message wake.** A Codex seat stopped because no claimable graph step
  existed (`cross-codex-head-20260926-a`).
- **Codex work wake reliability.** The eval controller received a signal and cancelled its own
  revision (`wake-codex-head-20260926-a`).
- **Claude restart continuity.** The Claude worker submitted a parent before its nested steps
  (`restart-sonnet-head-20260926-a`).

## Baseline `9b3c0a3`

| Eval | omp | Codex `gpt-6-luna` | Claude `claude-sonnet-5` |
| --- | --- | --- | --- |
| Seat mission work | pass after an eval fix: 3 wakes, claim 54.7 s | pass: claim 15.3 s | pass: claim 4.0 s |
| Work wake reliability | pass: median claim 8.5 s | pass: median claim 4.5 s | pass: median claim 2.2 s |
| Cross-harness message wake | fail: one omp seat spoke as the Codex seat | pass | pass |
| Restart continuity | fail: rc.dev finished; the Claude supervisor stalled | fail: the supervisor stalled | fail ×3: the worker stalled |

## omp failures and their status

1. **Fixed: the session start used st2 vocabulary.** The pi-family channel opened every omp session
   with "Set your status to available ... Set busy before work". st3 has no status command, so omp
   seats searched for one before they claimed work. `93a515a` replaced it with the st3 boot
   contract and the seat's own identity.
2. **Fixed: one omp seat spoke as another agent.** omp runs its Python `eval` tool with an
   allowlisted environment that drops `ST_AGENT`, `ST3_BIN`, and `ST3_ENDPOINT`. In
   `cross-omp-20260926-a`, one seat probed its identity there, decided that it was the Codex seat,
   and sent `FACT` and `AGREEMENT` as that seat. `93a515a` names the seat in the session context.
   `0281690` makes the CLI refuse another agent's identity from a process whose `ST_AGENT` names a
   seat. No later omp run sent a message under another identity.
3. **Fixed: duplicate wakes interrupted the boot turn.** The first wake arrived while omp's boot turn
   was already `working`. omp recorded delivery into that turn, but the reconciler waited for a read,
   a close, or a new `working` edge. It sent two more wakes, each of which interrupted the turn.
   `93a515a` acknowledges delivery into a working turn. Every later omp assignment needed one wake.
4. **Fixed: nested work after an early parent submission.** An agent could submit a parent step
   before its nested steps. st3 then held the parent `verifying` and never woke the idle seat for
   the nested work. `ed38d1f` wakes that nested step. `0e0358e` first waits until the submitting
   turn has ended, so an omp seat that continues in the same turn draws no extra message.
5. **Fixed: a missing declared product left no trace.** In `restart-ompfix-20260926-a`, omp recorded
   its product under a doubled `resource/mission-run/mission-run/...` subject, and the step waited in
   silence. `f6c76b6` tells the idle worker which exact subject and fields the step waits for.
6. **Improved: a person option rejected the seat's own identity.** omp often ran `now --as
   $ST_AGENT` and read the person-authority refusal as a refusal of its identity everywhere. `0c7a764`
   names the agent commands in that refusal, and `0281690` accepts `conversations ls --as`.
7. **Not fixed: a steered message can background a running command.** omp backgrounds an in-flight
   shell or eval call when a steered message arrives. In `cross-omp-head-20260926-c` the model then
   repeated the send whose result it had not seen. Two alternatives made omp worse and were reverted
   (see below).
8. **Model behavior, not a runtime fault.** Some omp runs skipped a protocol step, dropped required
   tags, did restart items from the parent goal, or left the injected duplicate unarchived. Codex
   `gpt-6-luna` made the same kinds of slip. Each report lists its evidence.

## Fixes on this branch and their proof

Each fix has a test that fails without it. A mutation run reverted each fix in place, confirmed
that its test failed, and restored the source.

| Commit | Change | Test that fails without it |
| --- | --- | --- |
| `93a515a` | pi-family session context uses the st3 boot contract and names the seat | `pi_family_session_ritual_uses_only_the_st3_boot_contract` |
| `93a515a` | a wake delivered into a working turn is acknowledged | `a_wake_delivered_into_an_already_working_turn_is_acknowledged` |
| `ed38d1f` | an early parent submission frees the seat for ready nested work | `inherited_nested_work_keeps_one_parent_alert` |
| `0e0358e` | that nested wake waits for the working turn to end | `inherited_nested_work_keeps_one_parent_alert` |
| `0281690` | a harness process cannot act as another agent | `a_harness_cannot_act_as_another_agent` |
| `0281690` | `conversations ls --as` names the mailbox, as `conversations read --as` does | `conversations_ls_accepts_the_read_spelling_of_its_mailbox` |
| `f6c76b6` | an idle worker learns which declared product its step waits for | `a_worker_report_waits_for_the_declared_product` |
| `0c7a764` | a person option names the agent commands when given an agent | `a_person_option_points_an_agent_to_its_own_commands` |
| `55c777b` | pi-family frames use the shared envelope and the steer boundary | `pi_family_mail_uses_the_shared_envelope_and_the_steer_boundary` |

The evals were repaired as well. `22158cd` states the seat-mission-work artifact content that its
held-out gate checks. `3eaf957` fixes the restart injector and graph judge for the current CLI.
`8dd9cbb` excludes st3's `.st3/` boot directory from the ledger's clean-worktree check. `22158cd`
adds `variants/` and `WAKE_PAIRS`.

`cargo test -p st3` passes: 420 library, 93 CLI, 5 client CLI, 21 client contract, 27 example and
eval, and 12 operational-state tests.

## Changes tried and reverted

| Change | Measured result | Reverted in |
| --- | --- | --- |
| Boot contract lists work with `work ls --as "$ST_AGENT"` (`0281690`) | Hid the running agentless controller step. Codex seats then asked for person action instead of running the message protocol | `72c04ce` |
| Boot contract adds claim, mailbox, and nested-order lines (`0281690`, `72c04ce`) | No measurable benefit: the Claude worker still submitted parents early. It was reverted while Codex stops were suspected, and those stops later recurred on the baseline text | `21a507e` |
| omp mail queued with `followUp` instead of steered (`c8288a8`) | No tool call was backgrounded. But the queue re-delivered messages omp had already read, and seats then declined the next task. omp caused failures in 4 of 6 cross-harness runs, against 2 of 10 steered runs before it | `55c777b` |
| Unread mail re-delivered once at each idle edge (`0d9cdc4`) | All four omp cross-harness runs failed the same way (omp skipped its agreement to Codex), and both restart runs failed. The transcripts show no duplicate prompt reaching omp, so the mechanism is not established | `46fe831` |

## Message envelope

omp already receives the same envelope as Codex and Claude. The pi channel, the Codex driver, and
the Claude channel all call `st2::ding::st3_notification_text`. It renders
`[PING from st3] message/ID from SENDER: TITLE` and a bounded preview of the body. The omp
transcripts show that header on every delivery. The earlier note that omp received only the
subject and body does not describe this branch.

## Not fixed

- **Cross-harness message wake conflicts with the boot contract.** The kickoff message says "This
  message is the work". The boot contract says person-authorized work must be claimable graph work,
  and otherwise the agent should request person action. Codex seats, and sometimes omp seats, stop
  on that rule. The eval could expose each participant's work as a claimable step, or the boot
  contract could treat explicit message-only coordination as authorized.
- **A steered message backgrounds omp's running command.** See failure 7. A fix probably belongs in
  the omp channel extension, which could hold a steer until the in-flight tool call returns.
- **An exec exit-code gate waits for its step timeout.** A `field "exit_code" ... is 0` gate stays
  pending after the exec exits non-zero. The exec observation does not carry its restart policy, so
  failing at once needs the desired exec spec.
- **omp's Python tool still lacks `ST_AGENT` and `ST3_ENDPOINT`.** A Python call to `st3` without
  `ST3_ENDPOINT` would reach the default local socket.
- **Claude sonnet submits restart parents early.** The Claude worker submitted a parent before its
  nested steps in every attempt that reached one. The restart coordination gate counts every runtime
  work message as an assignment, so each resulting nested wake fails it.
- **The work-wake controller's cleanup trap also fires on signals and then continues.** In
  `wake-codex-head-20260926-a` a signal of unknown origin cancelled its revisable run mid-scenario.

## Evidence notes

- The eval host's `/tmp` is RAM-backed. Each run kept a copy of the 430 MB debug binary, and `/tmp`
  filled at 23:24 UTC. The four runs that launched then are reported as stopped. A fifth,
  `restart-codex-final-20260926-b`, never created a mission run and has no report. The runner now
  deletes its binary copy at teardown.
- The commit message of `46fe831` gives a mechanism for the unread-mail failures that the
  transcripts do not support; this document and the reports state what was observed.
- None of these changes are deployed. The live fleet still runs its installed binary.
- No run used the committed four-harness cross-harness fixture, because Pi is not installed on this
  host.
- Each final configuration ran one to six times. These results are not a statistical measure of
  reliability.
