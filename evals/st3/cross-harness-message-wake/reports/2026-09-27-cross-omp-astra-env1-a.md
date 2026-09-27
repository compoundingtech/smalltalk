# Eval run report — 2026-09-27

- Eval: `cross-harness-message-wake` (fixed Codex gpt-6-sol and Claude opus seats, each paired with an omp seat on `openai-codex/gpt-6-astra`)
- Runtime: `st3`
- Run ID: `mission-run/cross-omp-astra-env1-a`
- Candidate commit: st3 binary built from `agent/message-envelope` `f824999a`: the `<smalltalk-message>` envelope for Codex, OpenCode, pi and omp and the new boot-contract sentence
- Eval KDL SHA-256: `0935dbe774a13dc1bf99b547c0dfee2df4b6a2124f3cac86ed7297805b54a3a7` (`variants/omp-astra.kdl`)
- Result: `fail`

## Timing

- Started: `2026-09-27T13:06:22.930Z`
- Ended: `2026-09-27T13:36:09.832Z`
- Duration: `1786.902`

## Model usage

| Role | Harness | Model | Tokens |
| --- | --- | --- | ---: |
| `wake.claude` | `claude` | `claude-opus-5-5 × 10 turns` | `409,549` |
| `wake.codex` | `codex` | `gpt-6-sol × 1 turns` | `231,265` |
| `wake.omp` | `omp` | `openai-codex/gpt-6-astra × 10 turns` | `206,174` |
| `wake.omp-2` | `omp` | `openai-codex/gpt-6-astra × 7 turns` | `140,323` |

- Agent tokens: `987,311`
- LLM judge tokens: `0` (no LLM judge)
- Total model tokens: `987,311`
- Count source: provider transcripts. Models are counted per assistant turn in each transcript. Tokens are omp `usage.totalTokens` summed per turn, Codex final `total_token_usage.total_tokens`, and Claude input + output + cache-read + cache-creation tokens summed per turn; cached reads dominate. The mission projection reported `usage: null`.

## Observable graph transitions

| Time | Subject or step | Transition | Evidence |
| --- | --- | --- | --- |
| `13:06:22.930` | `mission-run/cross-omp-astra-env1-a` | `absent -> created` | store index 3, mission-run.created |
| `13:06:22.951` | `exercise-message-wake` | `absent -> ready` | store index 9, step-run.state |
| `13:36:08.834` | `exercise-message-wake` | `working -> cancelled` | store index 118, step-run.state |
| `13:36:08.834` | `held-out-gates` | `absent -> cancelled` | store index 119, step-run.state |
| `13:36:08.867` | `cleanup-agents` | `absent -> ready` | store index 123, step-run.state |
| `13:36:09.810` | `cleanup-agents` | `working -> completed` | store index 145, step-run.state |
| `13:36:09.832` | `mission-run/cross-omp-astra-env1-a` | `running -> cancelled` | store index 148, mission-run.state |

## Small Talk

- Runtime work messages: `0`
- Direct agent messages: `7`
- Person or controller messages: `4`
- Required sequence: per phase: the controller sends 4 kickoffs; each pair exchanges one `FACT`, then one matching `AGREEMENT`; each participant sends one `CONSENSUS` to `person/eval-requester`. The idle phase starts only after all four seats report exactly `idle`
- Unexpected or duplicate messages: none (4 kickoffs, 7 protocol messages)

## Judges

| Judge | Result | Duration | Evidence |
| --- | --- | ---: | --- |
| none reached | `n/a` | `n/a` | the mission stopped before any gate ran |
| terminal input audit | `pass` | `n/a` | `0` `terminal.input.requested` claims in the whole graph |

## Result details

- Products or commits: none beyond the graph records above
- Cleanup: complete
- Notable behavior: Startup: kickoffs sent at 13:06:26.361; all receipts +6.2 s; fact stage never completed.
- Notable behavior: Provider transcripts contain 11 `<smalltalk-message id=` and 14 `[PING from st3]` occurrences (the Claude seat's channel notices keep the PING form).
- Notable behavior: Attention requests: `wake.omp-2`: "Consensus startup requires claimable graph work" (that seat sent no protocol message).
- Notable behavior: The controller exited 1 when the startup fact stage timed out after 300 s; the run was cancelled at 13:36 UTC instead of waiting for the 35-minute step timeout. The Codex/omp pair finished its startup exchange. `wake.omp-2` read its kickoff and Claude's `FACT QUARTZ`, tried to claim the agentless controller step, and asked for claimable work: "Please expose an eligible consensus step for this agent so the protocol can proceed under boot graph-work requirements. No protocol messages sent." This is the boot-contract conflict with a message-only eval.
- omp failures:
  - `wake.omp-2` stopped at startup and requested claimable graph work instead of sending its fact (boot-contract stop, not a delivery fault)
- Follow-up: Expose each participant's protocol as a claimable step, or accept message-only coordination in the boot contract
