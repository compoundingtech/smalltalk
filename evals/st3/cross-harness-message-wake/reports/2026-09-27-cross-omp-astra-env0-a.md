# Eval run report — 2026-09-27

- Eval: `cross-harness-message-wake` (fixed Codex gpt-6-sol and Claude opus seats, each paired with an omp seat on `openai-codex/gpt-6-astra`)
- Runtime: `st3`
- Run ID: `mission-run/cross-omp-astra-env0-a`
- Candidate commit: st3 binary built from `main` `513fc98b`, before the envelope (baseline)
- Eval KDL SHA-256: `0935dbe774a13dc1bf99b547c0dfee2df4b6a2124f3cac86ed7297805b54a3a7` (`variants/omp-astra.kdl`)
- Result: `pass`

## Timing

- Started: `2026-09-27T13:03:32.032Z`
- Ended: `2026-09-27T13:05:32.817Z`
- Duration: `120.785`

## Model usage

| Role | Harness | Model | Tokens |
| --- | --- | --- | ---: |
| `wake.claude` | `claude` | `claude-opus-5-5 × 30 turns` | `1,296,354` |
| `wake.codex` | `codex` | `gpt-6-sol × 2 turns` | `497,493` |
| `wake.omp` | `omp` | `openai-codex/gpt-6-astra × 18 turns` | `361,535` |
| `wake.omp-2` | `omp` | `openai-codex/gpt-6-astra × 14 turns` | `294,653` |

- Agent tokens: `2,450,035`
- LLM judge tokens: `0` (no LLM judge)
- Total model tokens: `2,450,035`
- Count source: provider transcripts. Models are counted per assistant turn in each transcript. Tokens are omp `usage.totalTokens` summed per turn, Codex final `total_token_usage.total_tokens`, and Claude input + output + cache-read + cache-creation tokens summed per turn; cached reads dominate. The mission projection reported `usage: null`.

## Observable graph transitions

| Time | Subject or step | Transition | Evidence |
| --- | --- | --- | --- |
| `13:03:32.032` | `mission-run/cross-omp-astra-env0-a` | `absent -> created` | store index 3, mission-run.created |
| `13:03:32.053` | `exercise-message-wake` | `absent -> ready` | store index 9, step-run.state |
| `13:05:30.625` | `exercise-message-wake` | `working -> completed` | store index 205, step-run.state |
| `13:05:30.652` | `held-out-gates` | `absent -> ready` | store index 206, step-run.state |
| `13:05:31.752` | `held-out-gates` | `working -> completed` | store index 227, step-run.state |
| `13:05:31.807` | `cleanup-agents` | `absent -> ready` | store index 230, step-run.state |
| `13:05:32.796` | `cleanup-agents` | `working -> completed` | store index 252, step-run.state |
| `13:05:32.817` | `mission-run/cross-omp-astra-env0-a` | `running -> completed` | store index 255, mission-run.state |

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
| the canonical conversation proves wake, exchange, and consensus | `pass` | `0.816` | stage `held-out-gates` |
| no terminal input path participated | `pass` | `0.256` | stage `held-out-gates` |
| terminal input audit | `pass` | `n/a` | `0` `terminal.input.requested` claims in the whole graph |

## Result details

- Products or commits: none beyond the graph records above
- Cleanup: complete
- Notable behavior: Startup: kickoffs sent at 13:03:35.419; all receipts +6.2 s; fact stage complete +46.9 s; agreement stage complete +58.2 s; result stage complete +71.5 s.
- Notable behavior: Idle: kickoffs sent at 13:04:56.759; all receipts +3.2 s; fact stage complete +12.4 s; agreement stage complete +22.6 s; result stage complete +33.8 s.
- Notable behavior: Provider transcripts contain 0 `<smalltalk-message id=` and 48 `[PING from st3]` occurrences (baseline envelope).
- omp failures: none observed
- Follow-up: none
