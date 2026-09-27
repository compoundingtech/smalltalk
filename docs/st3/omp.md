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
makes the same kinds of slip.
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

## Setting up an omp seat

- **Version.** st3 admits omp 18.0 and 18.1. It runs `omp --version` before each launch and refuses
  any other minor release. These evals used 18.1.22.
- **Runtime.** omp's launcher runs under `bun`, so `bun` must be on the seat's `PATH`. It is not
  enough for it to be on your interactive `PATH`. st3 starts harnesses from the login shell
  environment.
- **Login.** st3 does not log omp in. The seat uses the credentials in the omp profile of the user
  who runs the st3 daemon. Log that profile in to the `openai-codex` provider before you start a
  seat.
- **Model.** Name a model that your login offers. The login tested here offered `gpt-5.5`,
  `gpt-5.6-luna`, `gpt-5.6-sol`, `gpt-5.6-terra`, and `gpt-6-astra`, but not `gpt-6-luna`.
  omp fuzzy-matches model names, so an unavailable name can silently select another model. Confirm
  the model in the session transcript: its `model_change` entry and each assistant turn record the
  provider and model.
- **Declaration.**

  ```kdl
  agent "worker" {
    workspace "${ST_WORKSPACE}"
    harness "omp" {
      model "openai-codex/gpt-5.6-luna"
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
