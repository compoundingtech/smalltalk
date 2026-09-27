# Eval run report — 2026-09-27

- Eval: `cross-harness-message-wake` (fixed Codex gpt-6-sol and Claude opus seats, each paired with an omp seat on `openai-codex/gpt-6-astra`)
- Runtime: `st3`
- Run ID: `mission-run/cross-omp-envctl-astra-20260927-a`
- Candidate commit: st3 binary built from `main` `513fc98b`, before the envelope (baseline)
- Eval KDL SHA-256: `0935dbe774a13dc1bf99b547c0dfee2df4b6a2124f3cac86ed7297805b54a3a7` (`variants/omp-astra.kdl`)
- Result: `stopped`

## Timing

- Started: `2026-09-27T12:46:06.029Z`
- Ended: `2026-09-27T13:03:10.858Z`
- Duration: `1024.829`

## Model usage

| Role | Harness | Model | Tokens |
| --- | --- | --- | ---: |
| `wake.omp` | `omp` | `openai-codex/gpt-6-astra × 6 turns` | `115,846` |

- Agent tokens: `115,846`
- LLM judge tokens: `0` (no LLM judge)
- Total model tokens: `115,846`
- Count source: provider transcripts. Models are counted per assistant turn in each transcript. Tokens are omp `usage.totalTokens` summed per turn, Codex final `total_token_usage.total_tokens`, and Claude input + output + cache-read + cache-creation tokens summed per turn; cached reads dominate. The mission projection reported `usage: null`.

## Observable graph transitions

| Time | Subject or step | Transition | Evidence |
| --- | --- | --- | --- |
| `12:46:06.029` | `mission-run/cross-omp-envctl-astra-20260927-a` | `absent -> created` | store index 3, mission-run.created |
| `12:46:06.054` | `exercise-message-wake` | `absent -> ready` | store index 9, step-run.state |
| `13:03:09.671` | `exercise-message-wake` | `working -> cancelled` | store index 4649, step-run.state |
| `13:03:09.671` | `held-out-gates` | `absent -> cancelled` | store index 4650, step-run.state |
| `13:03:10.054` | `cleanup-agents` | `absent -> ready` | store index 4656, step-run.state |
| `13:03:10.732` | `cleanup-agents` | `working -> completed` | store index 4670, step-run.state |
| `13:03:10.858` | `mission-run/cross-omp-envctl-astra-20260927-a` | `running -> cancelled` | store index 4674, mission-run.state |

## Small Talk

- Runtime work messages: `0`
- Direct agent messages: `0`
- Required sequence: per phase: the controller sends 4 kickoffs; each pair exchanges one `FACT`, then one matching `AGREEMENT`; each participant sends one `CONSENSUS` to `person/eval-requester`. The idle phase starts only after all four seats report exactly `idle`
- Unexpected or duplicate messages: none (0 kickoffs, 0 protocol messages)

## Judges

| Judge | Result | Duration | Evidence |
| --- | --- | ---: | --- |
| none reached | `n/a` | `n/a` | the mission stopped before any gate ran |
| terminal input audit | `pass` | `n/a` | `0` `terminal.input.requested` claims in the whole graph |

## Result details

- Products or commits: none beyond the graph records above
- Cleanup: complete
- Notable behavior: Provider transcripts contain 0 `<smalltalk-message id=` and 3 `[PING from st3]` occurrences (baseline envelope).
- Notable behavior: Attention requests: `reconciler`: "A Codex agent stopped after repeated failures" (runtime).
- Notable behavior: Eval fault, not a harness or model result: the fixed Codex seat `wake.codex` failed to launch three times because its PTY session name `cross-omp-envctl-astra-20260927-a.wake.codex` made a socket path of 105 bytes, over the 104-byte limit. st3 raised "A Codex agent stopped after repeated failures". The runner cancelled the run at 13:03 UTC and it was repeated under a shorter run ID. It is excluded from the before/after counts.
- omp failures: none observed
- Follow-up: Runner: keep run IDs short enough that the PTY socket path stays under 104 bytes
