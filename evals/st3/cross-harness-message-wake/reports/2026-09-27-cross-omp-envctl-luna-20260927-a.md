# Eval run report — 2026-09-27

- Eval: `cross-harness-message-wake` (fixed Codex gpt-6-sol and Claude opus seats, each paired with a omp seat on `openai-codex/gpt-5.6-luna`)
- Runtime: `st3`
- Run ID: `mission-run/cross-omp-envctl-luna-20260927-a`
- Candidate commit: st3 binary built from `main` `513fc98b`, before the envelope (baseline)
- Eval KDL SHA-256: `0459220519955da16f8d62164c8a8c832734796e85d8696ce79ea910d99b1720` (`variants/omp-luna.kdl`)
- Result: `pass`

## Timing

- Started: `2026-09-27T12:46:01.945Z`
- Ended: `2026-09-27T12:47:40.773Z`
- Duration: `98.828`

## Model usage

| Role | Harness | Model | Tokens |
| --- | --- | --- | ---: |
| `wake.claude` | `claude` | `claude-opus-5-5 × 28 turns` | `1,242,022` |
| `wake.codex` | `codex` | `gpt-6-sol × 4 turns` | `387,223` |
| `wake.omp` | `omp` | `openai-codex/gpt-5.6-luna × 22 turns` | `485,869` |
| `wake.omp-2` | `omp` | `openai-codex/gpt-5.6-luna × 15 turns` | `302,010` |

- Agent tokens: `2,417,124`
- LLM judge tokens: `0` (no LLM judge)
- Total model tokens: `2,417,124`
- Count source: provider transcripts. Models are counted per assistant turn in each transcript. Tokens are omp `usage.totalTokens` summed per turn, Codex final `total_token_usage.total_tokens`, and Claude input + output + cache-read + cache-creation tokens summed per turn; cached reads dominate. The mission projection reported `usage: null`.

## Observable graph transitions

| Time | Subject or step | Transition | Evidence |
| --- | --- | --- | --- |
| `12:46:01.945` | `mission-run/cross-omp-envctl-luna-20260927-a` | `absent -> created` | store index 3, mission-run.created |
| `12:46:01.965` | `exercise-message-wake` | `absent -> ready` | store index 9, step-run.state |
| `12:47:38.487` | `exercise-message-wake` | `working -> completed` | store index 207, step-run.state |
| `12:47:38.518` | `held-out-gates` | `absent -> ready` | store index 208, step-run.state |
| `12:47:39.681` | `held-out-gates` | `working -> completed` | store index 227, step-run.state |
| `12:47:39.742` | `cleanup-agents` | `absent -> ready` | store index 230, step-run.state |
| `12:47:40.747` | `cleanup-agents` | `working -> completed` | store index 252, step-run.state |
| `12:47:40.773` | `mission-run/cross-omp-envctl-luna-20260927-a` | `running -> completed` | store index 255, mission-run.state |

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
| the canonical conversation proves wake, exchange, and consensus | `pass` | `0.893` | stage `held-out-gates` |
| no terminal input path participated | `pass` | `0.237` | stage `held-out-gates` |
| terminal input audit | `pass` | `n/a` | `0` `terminal.input.requested` claims in the whole graph |

## Result details

- Products or commits: none beyond the graph records above
- Cleanup: complete
- Notable behavior: Startup: kickoffs sent at 12:46:07.298; all receipts +9.3 s; fact stage complete +25.7 s; agreement stage complete +34.9 s; result stage complete +47.2 s.
- Notable behavior: Idle: kickoffs sent at 12:47:06.098; all receipts +3.2 s; fact stage complete +18.8 s; agreement stage complete +25.1 s; result stage complete +32.4 s.
- Notable behavior: Provider transcripts contain 0 `<smalltalk-message id=` and 50 `[PING from st3]` occurrences (baseline envelope).
- omp failures: none observed
- Follow-up: none
