# Eval run report — 2026-09-27

- Eval: `cross-harness-message-wake` (fixed Codex gpt-6-sol and Claude opus seats, each paired with an omp seat on `openai-codex/gpt-6-astra`)
- Runtime: `st3`
- Run ID: `mission-run/cross-omp-astra-env2-a`
- Candidate commit: st3 binary built from `agent/message-envelope` `f824999a` with `crates/st3/src/boot.rs` restored to `main`: the `<smalltalk-message>` envelope without the new boot-contract sentence (ablation)
- Eval KDL SHA-256: `0935dbe774a13dc1bf99b547c0dfee2df4b6a2124f3cac86ed7297805b54a3a7` (`variants/omp-astra.kdl`)
- Result: `pass`

## Timing

- Started: `2026-09-27T14:04:14.429Z`
- Ended: `2026-09-27T14:06:33.470Z`
- Duration: `139.041`

## Model usage

| Role | Harness | Model | Tokens |
| --- | --- | --- | ---: |
| `wake.claude` | `claude` | `claude-opus-5-5 × 33 turns` | `1,436,098` |
| `wake.codex` | `codex` | `gpt-6-sol × 2 turns` | `449,565` |
| `wake.omp` | `omp` | `openai-codex/gpt-6-astra × 19 turns` | `388,194` |
| `wake.omp-2` | `omp` | `openai-codex/gpt-6-astra × 15 turns` | `325,300` |

- Agent tokens: `2,599,157`
- LLM judge tokens: `0` (no LLM judge)
- Total model tokens: `2,599,157`
- Count source: provider transcripts. Models are counted per assistant turn in each transcript. Tokens are omp `usage.totalTokens` summed per turn, Codex final `total_token_usage.total_tokens`, and Claude input + output + cache-read + cache-creation tokens summed per turn; cached reads dominate. The mission projection reported `usage: null`.

## Observable graph transitions

| Time | Subject or step | Transition | Evidence |
| --- | --- | --- | --- |
| `14:04:14.429` | `mission-run/cross-omp-astra-env2-a` | `absent -> created` | store index 3, mission-run.created |
| `14:04:14.452` | `exercise-message-wake` | `absent -> ready` | store index 9, step-run.state |
| `14:06:31.375` | `exercise-message-wake` | `working -> completed` | store index 205, step-run.state |
| `14:06:31.403` | `held-out-gates` | `absent -> ready` | store index 206, step-run.state |
| `14:06:32.396` | `held-out-gates` | `working -> completed` | store index 224, step-run.state |
| `14:06:32.449` | `cleanup-agents` | `absent -> ready` | store index 227, step-run.state |
| `14:06:33.449` | `cleanup-agents` | `working -> completed` | store index 249, step-run.state |
| `14:06:33.470` | `mission-run/cross-omp-astra-env2-a` | `running -> completed` | store index 252, mission-run.state |

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
| the canonical conversation proves wake, exchange, and consensus | `pass` | `0.745` | stage `held-out-gates` |
| no terminal input path participated | `pass` | `0.219` | stage `held-out-gates` |
| terminal input audit | `pass` | `n/a` | `0` `terminal.input.requested` claims in the whole graph |

## Result details

- Products or commits: none beyond the graph records above
- Cleanup: complete
- Notable behavior: Startup: kickoffs sent at 14:04:18.004; all receipts +5.2 s; fact stage complete +52.1 s; agreement stage complete +62.4 s; result stage complete +74.7 s.
- Notable behavior: Idle: kickoffs sent at 14:05:52.301; all receipts +3.2 s; fact stage complete +15.4 s; agreement stage complete +28.8 s; result stage complete +39.0 s.
- Notable behavior: Provider transcripts contain 24 `<smalltalk-message id=` and 28 `[PING from st3]` occurrences (the Claude seat's channel notices keep the PING form).
- omp failures: none observed
- Follow-up: none
