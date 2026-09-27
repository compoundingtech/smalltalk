# Eval run report — 2026-09-27

- Eval: `cross-harness-message-wake` (fixed Codex gpt-6-sol and Claude opus seats, each paired with an omp seat on `openai-codex/gpt-6-astra`)
- Runtime: `st3`
- Run ID: `mission-run/cross-omp-hold-astra-20260927-b`
- Candidate commit: st3 binary built from `7e7d8d8`: the first omp mail hold (`9ba4c1b`) and the hook-root fix, on `agent/st3-next` `b138cd3`. This hold waited for omp's idle proof after a run and released held mail as separate steers; `c19aa71` and `c4f80ca` changed both
- Eval KDL SHA-256: `0935dbe774a13dc1bf99b547c0dfee2df4b6a2124f3cac86ed7297805b54a3a7` (`variants/omp-astra.kdl`)
- Result: `pass`

## Timing

- Started: `2026-09-27T08:38:06.701Z`
- Ended: `2026-09-27T08:39:38.633Z`
- Duration: `91.932`

## Model usage

| Role | Harness | Model | Tokens |
| --- | --- | --- | ---: |
| `wake.claude` | `claude` | `claude-opus-5-5 × 32 turns` | `1,426,908` |
| `wake.codex` | `codex` | `gpt-6-sol × 2 turns` | `414,242` |
| `wake.omp` | `omp` | `openai-codex/gpt-6-astra × 18 turns` | `357,731` |
| `wake.omp-2` | `omp` | `openai-codex/gpt-6-astra × 15 turns` | `316,136` |

- Agent tokens: `2,515,017`
- LLM judge tokens: `0` (no LLM judge)
- Total model tokens: `2,515,017`
- Count source: provider transcripts. Models are counted per assistant turn in each transcript. Tokens are omp `usage.totalTokens` summed per turn, Codex final `total_token_usage.total_tokens`, and Claude input + output + cache-read + cache-creation tokens summed per turn; cached reads dominate. The mission projection reported `usage: null`.

## Observable graph transitions

| Time | Subject or step | Transition | Evidence |
| --- | --- | --- | --- |
| `08:38:06.701` | `mission-run/cross-omp-hold-astra-20260927-b` | `absent -> created` | store index 3, mission-run.created |
| `08:38:06.727` | `exercise-message-wake` | `absent -> ready` | store index 9, step-run.state |
| `08:39:36.501` | `exercise-message-wake` | `working -> completed` | store index 204, step-run.state |
| `08:39:36.529` | `held-out-gates` | `absent -> ready` | store index 205, step-run.state |
| `08:39:37.623` | `held-out-gates` | `working -> completed` | store index 224, step-run.state |
| `08:39:37.677` | `cleanup-agents` | `absent -> ready` | store index 227, step-run.state |
| `08:39:38.611` | `cleanup-agents` | `working -> completed` | store index 249, step-run.state |
| `08:39:38.633` | `mission-run/cross-omp-hold-astra-20260927-b` | `running -> completed` | store index 252, mission-run.state |

## Small Talk

- Runtime work messages: `0`
- Direct agent messages: `24`
- Person or controller messages: `8`
- Required sequence: per phase: the controller sends 4 kickoffs; each pair exchanges one `FACT`, then one matching `AGREEMENT`; each participant sends one `CONSENSUS` to `person/eval-requester`. The idle phase starts only after all four seats report exactly `idle`
- Unexpected or duplicate messages: none: 8 kickoffs and exactly 24 protocol messages, each sent by its real seat

## Judges

| Judge | Result | Duration | Evidence |
| --- | --- | ---: | --- |
| the controller observed both final reports | `pass` | `n/a` | stage `exercise-message-wake` |
| the canonical conversation proves wake, exchange, and consensus | `pass` | `0.820` | stage `held-out-gates` |
| no terminal input path participated | `pass` | `0.244` | stage `held-out-gates` |
| terminal input audit | `pass` | `n/a` | `0` `terminal.input.requested` claims in the whole graph |

## Result details

- Products or commits: none beyond the graph records above
- Cleanup: complete
- Notable behavior: Startup: kickoffs sent at 08:38:10.529; receipts +6.2 s, facts +22.6 s, agreements +31.8 s, results +44.1 s. Idle: kickoffs sent at 08:39:01.547; receipts +3.2 s, facts +12.4 s, agreements +23.7 s, results +34.9 s.
- Notable behavior: Both omp `model_change` entries and every omp assistant turn record `openai-codex/gpt-6-astra`.
- Notable behavior: With the hold, no omp tool call was backgrounded by incoming mail (`wake.omp` 14 tool results; `wake.omp-2` 13 tool results). The omp channel handed the 11 messages for omp seats to omp a median 2.3 s after st3 staged them (at most 6.2 s); the omp seats read them a median 7.2 s after they were sent.
- omp failures: none observed
- Follow-up: none
