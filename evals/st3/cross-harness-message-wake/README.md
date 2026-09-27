# Cross-harness message wake

This paid black-box eval covers every account-backed interactive harness: Codex, Claude, Pi, and OMP. Codex and Claude form one consensus pair; Pi and OMP form another.

The native Codex/Claude lane deliberately tracks the canaried current models: `gpt-6-sol` and
Claude Code's `opus` alias. A live 2026-09-22 run recorded the latter as `claude-opus-5-5`.

One run exercises two deliberately separate delivery conditions:

1. `startup`: the controller sends as soon as every harness is running, reachable, and has a concrete native state. It records each exact pre-message state rather than treating `ready`, `active`, and `idle` as interchangeable.
2. `idle`: after the first protocol finishes, the controller waits until every harness reports exactly `idle` before sending the next message.

For each phase, every participant must consume a kickoff, send a private fact, wait for its peer's real fact, exchange a matching agreement, and independently report consensus. Native receipt latency has its own bound; each model-turn stage has a separate, larger bound. `controller-state.json` records those timestamps and preconditions.

The participant prompt describes only the coordination task. It does not mention historical failures, terminal workarounds, or the held-out checks.

Held-out gates prove exact message lifecycle, one canonical message per participant per stage, both paired results, exact idle preconditions for the idle phase, and absence of graph or executable terminal input.

`WAKE_PAIRS` selects the participant pairs for the controller and both judges; each name is the
agent `wake.<name>`. Its default is the committed `codex:claude pi:omp`. The files in `variants/`
pair the fixed Codex `gpt-6-sol` and Claude `opus` seats with two seats of one harness under test:
omp on `openai-codex/gpt-5.6-luna`, `openai-codex/gpt-5.6-sol`, or `openai-codex/gpt-6-astra`,
Codex `gpt-6-luna`, or Claude `claude-sonnet-5`. They need no Pi installation.
