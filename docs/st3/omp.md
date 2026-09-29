# Running st with omp

## Merge verdict

Merge `agent/omp-ready`. It fixes the omp failures that live evals exposed. One omp seat had spoken
for another agent, and duplicate wakes had interrupted omp's first turn. The branch also closes a
wake gap that stalled nested work for every harness and repairs three evals. Every fix has a test
that fails without it, and the `st` test suites pass. Nothing is deployed by the merge.

Expect omp to be reliable for assigned mission work and somewhat less reliable than Claude for
message-only coordination. On the final source, omp passed every seat-mission-work and
work-wake-reliability run, two of three restart-continuity runs, and three of six
cross-harness-message-wake runs. Most remaining failures are model slips by
`openai-codex/gpt-5.6-luna`, such as a skipped protocol step or a missing tag. Codex `gpt-6-luna`
makes the same kinds of slip. On `openai-codex/gpt-6-astra`, omp passed all three cross-harness
message wake runs; [Model choice](#model-choice) recommends that model.
[omp readiness evals](omp-readiness-2026-09-26.md) has the full evidence, and each run has a
report under `evals/st3/*/reports/`.

`agent/omp-steer-hold` adds a hold for mail that arrives while omp runs a turn, and two st fixes it
needs. [Holding mail during a running turn](#holding-mail-during-a-running-turn) has its verdict.

## What works

- **Durable seats and mission work.** An omp seat receives the work wake, claims its step with its
  live incarnation, and completes it. With the fixes, every assignment in the final runs needed one
  wake. Wake to claim took about 4–18 seconds; Codex is a few seconds faster and Claude faster
  still.
- **Revisions, replacement, and cancellation.** omp worked through two live mission revisions and a
  hangup-driven replacement incarnation, and claimed post-restart work on the first wake.
- **Restart continuity.** omp resumed an ordered ledger after a cold restart without repeating an
  item. In passing runs it also read and closed the injected duplicate work message.
- **Cross-harness messaging.** omp exchanged durable messages with Codex and Claude seats, both
  while starting and from exact idle, with no terminal input.

## What this branch fixed for omp

- The pi-family session context no longer tells omp to "set your status to available" and "set busy
  before work". st has no such commands, and omp searched for them before it claimed work. The
  branch replaced them with the st boot contract. st no longer adds instructions to that context;
  it carries only the seat's saved context.
- A CLI process whose `ST_AGENT` names a seat can no longer act as a different agent. omp's Python
  tool cannot see `ST_AGENT`. One omp seat probed its identity there, decided that it was the Codex
  seat, and sent protocol messages as that seat.
- A wake delivered into omp's already-running turn now counts as acknowledged. Before, st
  sent two more wakes 15 seconds apart, and each one interrupted omp's tool calls.
- A seat that submits a parent step before its nested steps now receives a wake for the ready nested
  step once its turn ends. Before, the run stalled.
- An idle worker whose submitted step waits on a declared product is told the exact subject and
  fields that step expects.
- `conversations ls --as MAILBOX` works. A person option given an agent subject now names the agent
  commands instead of a bare person-authority error.

## Model choice

Use `openai-codex/gpt-6-astra` for omp seats. It passed every cross-harness message wake run, and
its seats worked through the message protocol with the fewest actions and tokens of the three models
tested. It is a few seconds slower per protocol stage. If your login does not offer `gpt-6-astra`,
use `openai-codex/gpt-5.6-sol`.

The comparison uses cross-harness message wake on the final st behavior. Each run paired the same
fixed Codex `gpt-6-sol` and Claude `opus` seats with two omp seats at effort `medium`. The
`gpt-5.6-sol` and `gpt-6-astra` runs used a binary built from `f627df7`, whose st source is
identical to `55c777b`. The `gpt-5.6-luna` column counts the six final-behavior runs in
[omp readiness evals](omp-readiness-2026-09-26.md). In every run, each omp assistant turn in the
transcript records the named model.

| Cross-harness message wake | `gpt-5.6-luna` | `gpt-5.6-sol` | `gpt-6-astra` |
| --- | --- | --- | --- |
| Runs passed | 3 of 6 | 3 of 4 | 3 of 3 |
| Failures caused by an omp seat | 2 | 0 | 0 |
| Failures caused by the fixed Codex seat | 1 | 1 | 0 |
| omp seats that tried to claim or complete the controller's step | 6 of 12 | 2 of 8 | 0 of 6 |
| Median tool calls per omp seat | 24 | 30 | 15 |
| Median tool calls per omp seat backgrounded by incoming mail | 6 | 8 | 3 |
| Median output tokens per omp seat | 2,963 | 2,974 | 1,486 |
| Median reasoning tokens per omp seat | 632 | 729 | 18 |
| Median total tokens per omp seat | 457,622 | 467,009 | 387,824 |
| Median startup phase, kickoff to all four results | 44.0 s | 51.1 s | 58.3 s |
| Median idle phase, kickoff to all four results | 27.8 s | 31.8 s | 35.9 s |

The seat and phase medians cover passing runs only.

Why `gpt-6-astra`:

- **No omp slips.** No astra seat overlooked a message, repeated a send, or skipped a stage. The two
  luna failures were omp slips. One seat overlooked an agreement steered into its running turn and
  never sent its consensus (`cross-omp-end-20260926-b`). Another sent a fact twice after a steered
  message backgrounded the first send (`cross-omp-head-20260926-c`). Sol seats also made no slip.
- **It stays on the protocol.** Astra seats read, sent, and archived their messages with a median
  of 15 tool calls, against 30 for sol and 24 for luna. No astra seat tried to claim or complete the
  controller's agentless step. Two sol seats and six luna seats did, and st refused each attempt.
  With fewer calls in flight, fewer were backgrounded by incoming mail, which is the path behind
  luna's duplicate send. No backgrounded send was repeated in the sol or astra runs.
- **It costs less.** Astra seats used 15 to 17 percent fewer total tokens than sol or luna seats and
  half their output tokens. At effort `medium` they reported 11 to 30 reasoning tokens each.
- **It is slower.** Astra's median phases took 4 to 7 seconds longer than sol's and 8 to 14 seconds
  longer than luna's. Every stage still finished far inside the eval's 300-second stage deadline.

Limits:

- Three to six runs per model cannot prove a difference in reliability. The recommendation also
  rests on the consistent differences in how the seats behaved.
- Only cross-harness message wake ran on sol and astra. Seat mission work, work wake reliability,
  and restart continuity ran only on luna.
- The Codex failures are the boot-contract stop described under
  [What an operator should know](#what-an-operator-should-know), not omp results. Sol received a
  fourth run to replace its Codex failure.

Each run has a report in `evals/st3/cross-harness-message-wake/reports/`. The sol and astra runs
are `cross-omp-sol-20260927-a` to `-d` and `cross-omp-astra-20260927-a` to `-c`, from
`variants/omp-sol.kdl` and `variants/omp-astra.kdl`.

## Holding mail during a running turn

**Keep it, with the two st fixes it needs.** On `agent/omp-steer-hold`, the omp channel extension
holds a message that arrives while omp runs a turn and hands it to omp as the turn's tool batch
returns. In the final runs no omp tool call was backgrounded by incoming mail: 0 of 348, against 90
of 354 in the runs before the hold. No omp seat repeated a send. Pass rates did not change
measurably, and the remaining luna failures are model protocol slips, as before. In same-time paired
runs the phases were no slower, but omp seats read their mail a few seconds later.

### What changed

- **The hold** (`hooks/omp-channel.ts`). A message that arrives between `agent_start` and the end of
  the run, or while a tool call is in flight, is held. It is steered from the `tool_result` handler
  of the batch's last call. omp announces every call of a batch with `tool_call` before any of them
  runs, and it awaits that handler, so the steer is queued before omp looks for one at that
  boundary. Mail still held when the run ends is handed over at `agent_end`. Mail released together
  is one steer, because omp injects one queued steer per boundary. A message waits at most 10
  seconds behind a running tool call; then it is steered, and omp backgrounds the command as
  before. Ten seconds was chosen below st's 15-second work-wake retry. The cap does not bound the
  wait while the model streams, where omp would not inject the message sooner anyway. Outside a
  running turn, mail is handed over at once, as before.
- **Why the whole turn, not only in-flight calls.** In the 23 omp cross-harness runs before the
  hold, all 176 backgrounded calls started after their steer was queued, while the model was still
  streaming; none was running when the steer arrived. omp signals every shell and eval call of a
  batch that starts with a steer queued, and each backgrounds itself at once. Holding only while a
  call is in flight would have changed none of those 176 calls.
- **Hook root** (`src/hooks.rs`, `7e7d8d8`). st exports `ST_HOOKS` to its members as its binary's
  hook set directory, because its Claude settings run `$ST_HOOKS/claude-observe.sh`. The pi-family
  launcher read that directory as the hook root and verified `<set>/sets/<set>/`. omp seats
  launched only because the unchanged hook set held a stray nested copy of itself. Any change to a
  hook file, this one included, made every omp and pi seat under st fail with `launch-error`. The
  hook root now recognizes a set directory and resolves the root it names.
- **Late delivery acknowledgement** (`crates/st3/src/main.rs`, `7b9d489`). A seat can read a message
  through the CLI, which records delivery and the read, before its channel acknowledges the
  handoff. st refuses that late `read -> delivered` transition, and the pi-family channel exited on
  the refusal. The seat then had no channel: it received no mail, and st never saw it idle again.
  The hold widened this race from milliseconds to seconds, and it stalled two astra runs. The
  channel now treats that refusal as settled when the message already stands at delivered, read,
  or closed.

Each change has a test that fails without it: the omp extension smoke in `hooks/typecheck/`
(the `pi-extension-types` flake check), `an_exported_set_directory_names_its_hook_root`, and
`a_pi_family_delivery_after_the_recipient_read_the_message_keeps_the_channel`.

### Results

Cross-harness message wake, with the same fixed Codex `gpt-6-sol` and Claude `opus` partners. The
"before" columns are the runs behind [Model choice](#model-choice), from 2026-09-26/27. The final
columns ran `7b9d489`; later commits change only comments, documents, and reports.

| Cross-harness message wake | luna before | luna final | astra before | astra final |
| --- | --- | --- | --- | --- |
| Runs passed | 3 of 6 | 5 of 6 | 3 of 3 | 2 of 2 |
| Failures caused by an omp seat | 2 | 1 | 0 | 0 |
| Failures caused by the fixed Codex seat | 1 | 0 | 0 | 0 |
| omp tool calls backgrounded by incoming mail | 72 of 265 | 0 of 294 | 18 of 89 | 0 of 54 |
| Sends repeated after a backgrounded send | 1 | 0 | 0 | 0 |
| Median tool calls per omp seat, passing runs | 24 | 25.5 | 15 | 13.5 |
| Median total tokens per omp seat, passing runs | 457,622 | 400,610 | 387,824 | 343,261 |
| Median delay from staging to omp handoff | 0.0 s | 2.5 s | 0.0 s | 2.4 s |
| Median time from send to an omp seat's read | 7.2 s | 8.6 s | 6.0 s | 8.4 s |
| Median startup phase, kickoff to all four results | 44.0 s | 55.1 s | 58.3 s | 54.9 s |
| Median idle phase, kickoff to all four results | 27.8 s | 37.0 s | 35.9 s | 38.6 s |

Handoff and read times are the median of each run's median over messages to omp seats.

To separate the hold from the time of day, three control runs without the hold and three hold runs
ran side by side at 09:06 UTC. Their binaries differed only in `hooks/omp-channel.ts`. One hold run
hit the Codex boot-contract stop.

| Same-time luna runs | control, no hold | hold |
| --- | --- | --- |
| Runs passed | 3 of 3 | 2 of 3; the third was a Codex stop |
| omp tool calls backgrounded by incoming mail | 36 of 161 | 0 of 130 |
| Median startup phase, passing runs | 70.8 s | 61.4 s |
| Median idle phase, passing runs | 37.9 s | 36.5 s |
| Median time from send to an omp seat's read | 5.6 s | 9.5 s |

The phases were no slower with the hold, so the slower phases in the final runs, against the earlier
runs, come from the time of day rather than the hold. The hold's cost shows in how soon an omp seat
reads a message: a few seconds later, over two and three runs. The mechanism behind that delay was
not isolated.

Across all versions, 37 hold runs reached omp: 29 on luna and 8 on astra. On luna, 20 passed, 5
failed on omp protocol slips, and 4 stopped on the Codex boot-contract stop. On astra, 6 passed, and
2 stalled on the refused late acknowledgement that `7b9d489` fixes. Only 3 of their 1,532 omp tool
calls were backgrounded by incoming mail, all in one run of the first version.

- **`7e7d8d8`**, the first hold. It waited for omp's idle proof after a run and released held mail
  as separate steers. Luna passed 5 of 9: one omp slip and three Codex stops. Astra passed 1 of 2:
  `cross-omp-hold-astra-20260927-a` stalled on the refused acknowledgement. In
  `cross-omp-hold-luna-20260927-m` two messages were steered together; omp injected the first, and
  the second backgrounded the next batch's three calls.
- **`c19aa71`** hands held mail over at `agent_end` instead of waiting for idle. Luna passed 3 of 5
  with two omp slips; astra passed 2 of 2. Its commit message blames the idle wait for the astra
  stall; the stall came from the refused acknowledgement.
- **`c4f80ca`** hands mail released together to omp as one steer. Luna passed 5 of 6 with one omp
  slip. Astra passed 1 of 2: `cross-omp-astra-h3-20260927-a` stalled on the refused acknowledgement
  again, which led to `7b9d489`.
- **`7b9d489`**, the final code, is the results table. The paired hold runs ran `1b3f5b1`, which
  matches it apart from comments.

The five luna slips were a skipped fact or a skipped agreement. The six luna runs before the hold
had two omp failures, a missed consensus and a repeated send. In every slip, the message that
preceded it reached omp at the batch boundary where an unheld steer would also have landed.

Four runs are void because no omp seat could launch before the hook-root fix
(`cross-omp-hold-luna-20260927-a` to `-d`). Three more are void for eval setup reasons: a Claude
seat that never became ready, and two run IDs too long for the Claude seat's PTY socket path.

Limits:

- Six final luna runs and two final astra runs cannot prove a change in pass rate. The evidence
  for keeping the hold is the backgrounding that it removes, which was consistent in every run.
- The final runs ran eight at a time on one host.
- The hold was not measured on omp 18.0, or with commands that run longer than 10 seconds.

### Including it in st

The extension is an asset embedded in the st binary, so it ships with a binary change and needs no
configuration. It must ship with `7e7d8d8` and `7b9d489`. Without the hook-root fix, every omp seat
fails to launch once the hook set changes. Without the acknowledgement fix, a seat can lose its
channel whenever the model reads a held message itself.

An isolated eval daemon installs its binary's hook set into the user's
`~/.local/state/st2/hooks` and selects it in `current.json`. st seats use their own binary's set,
so running seats are unaffected, but `st2 hooks verify` then reports the last eval's set.

Each run has a report in `evals/st3/cross-harness-message-wake/reports/`, named by run ID:
`cross-omp-hold-*`, `cross-omp-hold2-*` to `cross-omp-hold5-*`, `cross-omp-astra-h2-*` to
`cross-omp-astra-h4-*`, and `cross-omp-ctl-*`.

## Setting up an omp seat

- **Version.** st admits omp 18.0 and 18.1. It runs `omp --version` before each launch and refuses
  any other minor release. These evals used 18.1.22.
- **Runtime.** omp's launcher runs under `bun`, so `bun` must be on the seat's `PATH`. It is not
  enough for it to be on your interactive `PATH`. st starts harnesses from the login shell
  environment.
- **Login.** st does not log omp in. The seat uses the credentials in the omp profile of the user
  who runs the st daemon. Log that profile in to the `openai-codex` provider before you start a
  seat.
- **Model.** Use `openai-codex/gpt-6-astra`; [Model choice](#model-choice) explains why. Name a
  model that your login offers. The login tested here offered `gpt-5.5`, `gpt-5.6-luna`,
  `gpt-5.6-sol`, `gpt-5.6-terra`, and `gpt-6-astra`, but not `gpt-6-luna`. omp fuzzy-matches model
  names, so an unavailable name can silently select another model. Confirm the model in the session
  transcript: its `model_change` entry and each assistant turn record the provider and model.
- **Declaration.**

  ```kdl
  agent "worker" {
    workspace "${ST_WORKSPACE}"
    harness "omp" {
      model "openai-codex/gpt-6-astra"
      effort "medium"
    }
  }
  ```

  `effort` becomes omp's `--thinking` level. st adds the channel extension and the session
  directory itself; do not pass them in `args`. omp starts with no prompt and stays idle until a
  person types or a message is posted.
- **Transcripts.** st points omp's `--session-dir` into the daemon's driver state for that seat.
  Look for the session JSONL there, not in omp's default session directory.

## What an operator should know

- omp receives the same `<smalltalk-message>` envelope as Codex; Claude receives the plain
  `[PING from st3] message/ID from SENDER: TITLE` notice inside its channel tag. Without the hold, a message is steered into omp's running turn, and omp backgrounds every
  shell or eval call of the next tool batch and tells the model the command keeps running. Rarely,
  the model then repeats that command. With the hold on `agent/omp-steer-hold`, the message reaches
  omp as that batch returns, and no command is backgrounded unless it runs longer than 10 seconds.
  Queueing the message until the turn ended was measured and was worse.
- omp's Python `eval` tool runs with a filtered environment without `ST_AGENT`, `ST3_BIN`, or
  `ST3_ENDPOINT`. Run st commands from the shell tool. The CLI refuses another agent's identity.
- The cross-harness message wake eval hands out work only by message. In the runs recorded here,
  the boot contract asked for claimable graph work, so omp and Codex seats sometimes stopped and
  requested person action there. That was an eval and boot-contract mismatch, not an omp delivery
  fault. st no longer starts a seat with that contract.
- Each run's report records its model from the provider transcript and its token counts.

## Still open

- On `gpt-5.6-luna`, omp seats still sometimes skip a protocol step, such as a fact or an
  agreement. The hold did not measurably change how often.
- omp 18.1.22 also accepts `deliverAs: "aside"`, which adds a message to the next turn without
  interrupting a tool batch. It might replace the hold. It was not measured, and omp 18.0 was not
  checked for it.
- An exec exit-code gate waits for its step timeout after a non-zero exit.
- The restart coordination gate counts every runtime work message as an assignment, so a worker that
  needs a wake per nested step fails it even when its work is correct.

[omp readiness evals](omp-readiness-2026-09-26.md) lists these and the reverted experiments in full.
