# Eval run report — 2026-09-27

- Eval: `cross-harness-message-wake` (fixed Codex gpt-6-sol and Claude opus seats, each paired with an omp seat on `openai-codex/gpt-6-astra`)
- Runtime: `st3`
- Run ID: `mission-run/cross-omp-astra-env0-c`
- Candidate commit: st3 binary built from `main` `513fc98b`, before the envelope (baseline)
- Eval KDL SHA-256: `0935dbe774a13dc1bf99b547c0dfee2df4b6a2124f3cac86ed7297805b54a3a7` (`variants/omp-astra.kdl`)
- Result: `pass`

## Timing

- Started: `2026-09-27T13:51:06.788Z`
- Ended: `2026-09-27T13:53:54.433Z`
- Duration: `167.645`

## Model usage

| Role | Harness | Model | Tokens |
| --- | --- | --- | ---: |
| `wake.claude` | `claude` | `claude-opus-5-5 × 30 turns` | `1,321,361` |
| `wake.codex` | `codex` | `gpt-6-sol × 4 turns` | `455,707` |
| `wake.omp` | `omp` | `openai-codex/gpt-6-astra × 17 turns` | `364,754` |
| `wake.omp-2` | `omp` | `openai-codex/gpt-6-astra × 15 turns` | `296,850` |

- Agent tokens: `2,438,672`
- LLM judge tokens: `0` (no LLM judge)
- Total model tokens: `2,438,672`
- Count source: provider transcripts. Models are counted per assistant turn in each transcript. Tokens are omp `usage.totalTokens` summed per turn, Codex final `total_token_usage.total_tokens`, and Claude input + output + cache-read + cache-creation tokens summed per turn; cached reads dominate. The mission projection reported `usage: null`.

## Observable graph transitions

| Time | Subject or step | Transition | Evidence |
| --- | --- | --- | --- |
| `13:51:06.788` | `mission-run/cross-omp-astra-env0-c` | `absent -> created` | store index 3, mission-run.created |
| `13:51:06.809` | `exercise-message-wake` | `absent -> ready` | store index 9, step-run.state |
| `13:53:51.894` | `exercise-message-wake` | `working -> completed` | store index 210, step-run.state |
| `13:53:51.923` | `held-out-gates` | `absent -> ready` | store index 211, step-run.state |
| `13:53:53.123` | `held-out-gates` | `working -> completed` | store index 229, step-run.state |
| `13:53:53.185` | `cleanup-agents` | `absent -> ready` | store index 232, step-run.state |
| `13:53:54.410` | `cleanup-agents` | `working -> completed` | store index 254, step-run.state |
| `13:53:54.433` | `mission-run/cross-omp-astra-env0-c` | `running -> completed` | store index 257, mission-run.state |

## Small Talk

- Runtime work messages: `0`
- Direct agent messages: `24`
- Person or controller messages: `8`
- Required sequence: per phase: the controller sends 4 kickoffs; each pair exchanges one `FACT`, then one matching `AGREEMENT`; each participant sends one `CONSENSUS` to `person/eval-requester`. The idle phase starts only after all four seats report exactly `idle`
- Unexpected or duplicate messages: none (8 kickoffs, 24 protocol messages)

## Judges

| Judge | Result | Duration | Evidence |
| --- | --- | ---: | --- |
| the controller observed both final reports | `pass` | `n/a` | stage `exercise-message-wake` |
| the canonical conversation proves wake, exchange, and consensus | `pass` | `0.894` | stage `held-out-gates` |
| no terminal input path participated | `pass` | `0.274` | stage `held-out-gates` |
| terminal input audit | `pass` | `n/a` | `0` `terminal.input.requested` claims in the whole graph |

## Result details

- Products or commits: none beyond the graph records above
- Cleanup: complete
- Notable behavior: Startup: kickoffs sent at 13:51:10.245; all receipts +13.6 s; fact stage complete +60.6 s; agreement stage complete +66.9 s; result stage complete +100.5 s.
- Notable behavior: Idle: kickoffs sent at 13:52:57.188; all receipts +4.2 s; fact stage complete +26.3 s; agreement stage complete +32.7 s; result stage complete +54.7 s.
- Notable behavior: Provider transcripts contain 0 `<smalltalk-message id=` and 50 `[PING from st3]` occurrences (baseline envelope).
- omp failures: none observed
- Follow-up: none
