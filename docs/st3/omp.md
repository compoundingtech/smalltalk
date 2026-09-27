# Running st3 with omp

## Merge verdict

Merge `agent/omp-ready`. It fixes the omp failures that live evals exposed. One omp seat had spoken
for another agent, and duplicate wakes had interrupted omp's first turn. The branch also closes a
wake gap that stalled nested work for every harness and repairs three evals. Every fix has a test
that fails without it, and the `st3` test suites pass. Nothing is deployed by the merge.

Expect omp to be reliable for assigned mission work and somewhat less reliable than Claude for
message-only coordination. On the final source, omp passed every seat-mission-work and
work-wake-reliability run, two of three restart-continuity runs, and three of six
cross-harness-message-wake runs. Most remaining failures are model slips by
`openai-codex/gpt-5.6-luna`, such as a skipped protocol step or a missing tag. Codex `gpt-6-luna`
makes the same kinds of slip. On `openai-codex/gpt-6-astra`, omp passed all three cross-harness
message wake runs; [Model choice](#model-choice) recommends that model.
[omp readiness evals](omp-readiness-2026-09-26.md) has the full evidence, and each run has a
report under `evals/st3/*/reports/`.

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
  before work". st3 has no such commands, and omp searched for them before it claimed work. The
  context now restates the st3 boot contract and names the seat.
- A CLI process whose `ST_AGENT` names a seat can no longer act as a different agent. omp's Python
  tool cannot see `ST_AGENT`. One omp seat probed its identity there, decided that it was the Codex
  seat, and sent protocol messages as that seat.
- A wake delivered into omp's already-running boot turn now counts as acknowledged. Before, st3
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

The comparison uses cross-harness message wake on the final st3 behavior. Each run paired the same
fixed Codex `gpt-6-sol` and Claude `opus` seats with two omp seats at effort `medium`. The
`gpt-5.6-sol` and `gpt-6-astra` runs used a binary built from `f627df7`, whose st3 source is
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
  controller's agentless step. Two sol seats and six luna seats did, and st3 refused each attempt.
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

## Setting up an omp seat

- **Version.** st3 admits omp 18.0 and 18.1. It runs `omp --version` before each launch and refuses
  any other minor release. These evals used 18.1.22.
- **Runtime.** omp's launcher runs under `bun`, so `bun` must be on the seat's `PATH`. It is not
  enough for it to be on your interactive `PATH`. st3 starts harnesses from the login shell
  environment.
- **Login.** st3 does not log omp in. The seat uses the credentials in the omp profile of the user
  who runs the st3 daemon. Log that profile in to the `openai-codex` provider before you start a
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

  `effort` becomes omp's `--thinking` level. st3 adds the channel extension, the session directory,
  and the boot prompt itself; do not pass them in `args`.
- **Transcripts.** st3 points omp's `--session-dir` into the daemon's driver state for that seat.
  Look for the session JSONL there, not in omp's default session directory.

## What an operator should know

- omp receives the same `[PING from st3] message/ID from SENDER: TITLE` envelope as Codex and
  Claude. A message is steered into omp's running turn at its next tool boundary. When one arrives,
  omp backgrounds an in-flight shell or eval call and tells the model the command keeps running.
  Rarely, the model then repeats that command. Queueing the message until the turn ended was
  measured and was worse, so st3 keeps the steer.
- omp's Python `eval` tool runs with a filtered environment without `ST_AGENT`, `ST3_BIN`, or
  `ST3_ENDPOINT`. Run st3 commands from the shell tool. The session context says so, and the CLI
  refuses another agent's identity.
- The cross-harness message wake eval hands out work only by message. The boot contract asks for
  claimable graph work, so omp and Codex seats sometimes stop and request person action there. That
  is an eval and boot-contract mismatch, not an omp delivery fault.
- Each run's report records its model from the provider transcript and its token counts.

## Still open

- Holding a steer until omp's in-flight tool call returns would need a change in the omp channel
  extension.
- An exec exit-code gate waits for its step timeout after a non-zero exit.
- The restart coordination gate counts every runtime work message as an assignment, so a worker that
  needs a wake per nested step fails it even when its work is correct.

[omp readiness evals](omp-readiness-2026-09-26.md) lists these and the reverted experiments in full.
