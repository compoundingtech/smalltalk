# Eval run report — 2026-09-27

- Eval: `cross-harness-message-wake` (fixed Codex gpt-6-sol and Claude opus seats, each paired with an omp seat on `openai-codex/gpt-5.6-luna`)
- Runtime: `st3`
- Run ID: `mission-run/cross-omp-luna-env0-b`
- Candidate commit: st3 binary built from `main` `513fc98b`, before the envelope (baseline)
- Eval KDL SHA-256: `0459220519955da16f8d62164c8a8c832734796e85d8696ce79ea910d99b1720` (`variants/omp-luna.kdl`)
- Result: `fail`

## Timing

- Started: `2026-09-27T13:36:26.248Z`
- Ended: `2026-09-27T13:41:43.997Z`
- Duration: `317.749`

## Model usage

| Role | Harness | Model | Tokens |
| --- | --- | --- | ---: |
| `wake.claude` | `claude` | `claude-opus-5-5 × 24 turns` | `1,013,762` |
| `wake.codex` | `codex` | `gpt-6-sol × 1 turns` | `155,273` |
| `wake.omp` | `omp` | `openai-codex/gpt-5.6-luna × 9 turns` | `188,665` |
| `wake.omp-2` | `omp` | `openai-codex/gpt-5.6-luna × 19 turns` | `450,123` |

- Agent tokens: `1,807,823`
- LLM judge tokens: `0` (no LLM judge)
- Total model tokens: `1,807,823`
- Count source: provider transcripts. Models are counted per assistant turn in each transcript. Tokens are omp `usage.totalTokens` summed per turn, Codex final `total_token_usage.total_tokens`, and Claude input + output + cache-read + cache-creation tokens summed per turn; cached reads dominate. The mission projection reported `usage: null`.

## Observable graph transitions

| Time | Subject or step | Transition | Evidence |
| --- | --- | --- | --- |
| `13:36:26.248` | `mission-run/cross-omp-luna-env0-b` | `absent -> created` | store index 3, mission-run.created |
| `13:36:26.268` | `exercise-message-wake` | `absent -> ready` | store index 9, step-run.state |
| `13:41:43.004` | `exercise-message-wake` | `working -> cancelled` | store index 112, step-run.state |
| `13:41:43.005` | `held-out-gates` | `absent -> cancelled` | store index 113, step-run.state |
| `13:41:43.031` | `cleanup-agents` | `absent -> ready` | store index 117, step-run.state |
| `13:41:43.974` | `cleanup-agents` | `working -> completed` | store index 139, step-run.state |
| `13:41:43.997` | `mission-run/cross-omp-luna-env0-b` | `running -> cancelled` | store index 142, mission-run.state |

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
- Notable behavior: Startup: kickoffs sent at 13:36:30.175; all receipts +9.3 s; fact stage never completed.
- Notable behavior: Provider transcripts contain 0 `<smalltalk-message id=` and 30 `[PING from st3]` occurrences (baseline envelope).
- Notable behavior: Attention requests: `wake.codex`: "Claimable work needed for consensus startup" (that seat sent no protocol message).
- Notable behavior: The controller exited 1 when the fact stage of the startup phase timed out after 300 s; the runner then cancelled the run instead of waiting for the 35-minute step timeout.
- omp failures: none observed
- Follow-up: none
