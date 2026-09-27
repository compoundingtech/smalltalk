# Eval run report — 2026-09-27

- Eval: `cross-harness-message-wake` (fixed Codex gpt-6-sol and Claude opus seats, each paired with an omp seat on `openai-codex/gpt-6-astra`)
- Runtime: `st3`
- Run ID: `mission-run/cross-omp-astra-next-20260927-a`
- Candidate commit: st3 and st2 binaries built from `a058499` on `agent/st3-next`: `st3` `9b3c0a3` with `agent/seat-queue` `bdf4bb2` and `agent/omp-ready` `b6d172e` merged
- Eval KDL SHA-256: `0935dbe774a13dc1bf99b547c0dfee2df4b6a2124f3cac86ed7297805b54a3a7` (`variants/omp-astra.kdl`)
- Result: `pass`

## Timing

- Started: `2026-09-27T01:55:12.576Z`
- Ended: `2026-09-27T01:58:12.997Z`
- Duration: `180.421`

## Model usage

| Role | Harness | Model | Tokens |
| --- | --- | --- | ---: |
| `wake.claude` | `claude` | `claude-opus-5-5 × 28 turns` | `1,232,584` |
| `wake.codex` | `codex` | `gpt-6-sol × 2 turns` | `362,835` |
| `wake.omp` | `omp` | `openai-codex/gpt-6-astra × 19 turns` | `379,590` |
| `wake.omp-2` | `omp` | `openai-codex/gpt-6-astra × 16 turns` | `342,307` |

- Agent tokens: `2,317,316`
- LLM judge tokens: `0` (no LLM judge)
- Total model tokens: `2,317,316`
- Count source: provider transcripts. Models are counted per assistant turn in each transcript. Tokens are omp `usage.totalTokens` summed per turn, Codex final `total_token_usage.total_tokens`, and Claude input + output + cache-read + cache-creation tokens summed per turn; cached reads dominate. The mission projection reported `usage: null`.

## Observable graph transitions

| Time | Subject or step | Transition | Evidence |
| --- | --- | --- | --- |
| `01:55:12.576` | `mission-run/cross-omp-astra-next-20260927-a` | `absent -> created` | store index 3, mission-run.created |
| `01:55:12.598` | `exercise-message-wake` | `absent -> ready` | store index 9, step-run.state |
| `01:58:11.071` | `exercise-message-wake` | `working -> completed` | store index 304, step-run.state |
| `01:58:11.101` | `held-out-gates` | `absent -> ready` | store index 305, step-run.state |
| `01:58:12.239` | `held-out-gates` | `working -> completed` | store index 323, step-run.state |
| `01:58:12.292` | `cleanup-agents` | `absent -> ready` | store index 326, step-run.state |
| `01:58:12.976` | `cleanup-agents` | `working -> completed` | store index 348, step-run.state |
| `01:58:12.997` | `mission-run/cross-omp-astra-next-20260927-a` | `running -> completed` | store index 351, mission-run.state |

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
| the canonical conversation proves wake, exchange, and consensus | `pass` | `0.842` | stage `held-out-gates` |
| no terminal input path participated | `pass` | `0.263` | stage `held-out-gates` |
| terminal input audit | `pass` | `n/a` | `0` `terminal.input.requested` claims in the whole graph |

## Result details

- Products or commits: none beyond the graph records above
- Cleanup: complete
- Notable behavior: Startup: kickoffs sent at 01:55:16.041; receipts +10.2 s, facts +63.9 s, agreements +74.3 s, results +102.9 s. Idle: kickoffs sent at 01:57:31.176; receipts +4.2 s, facts +13.4 s, agreements +27.6 s, results +39.9 s.
- Notable behavior: The startup fact stage waited on the fixed Codex seat. It read its boot file 9 s after the kickoff, ran its next command 29 s later, and sent its fact 63 s after the kickoff. The Claude/omp pair reported consensus 42 s after the kickoff.
- Notable behavior: Both omp `model_change` entries and every omp assistant turn record `openai-codex/gpt-6-astra`.
- Notable behavior: Incoming messages backgrounded omp tool calls: `wake.omp` 3 (including 2 `conversations send` calls); `wake.omp-2` 4 (each including a `conversations send` call). None of those sends was repeated.
- Notable behavior: No omp seat claimed or completed the controller's agentless step. `wake.omp` read `work claim --help` once.
- omp failures: none observed
- Follow-up: none
