# Eval run report — 2026-09-27

- Eval: `cross-harness-message-wake` (fixed Codex gpt-6-sol and Claude opus seats, each paired with an omp seat on `openai-codex/gpt-6-astra`)
- Runtime: `st3`
- Run ID: `mission-run/cross-omp-hold2-astra-20260927-b`
- Candidate commit: st3 binary built from `c19aa71`: the omp mail hold handing held mail over at `agent_end`, and the hook-root fix, on `agent/st3-next` `b138cd3`. This hold still released several held messages as separate steers; `c4f80ca` changed that
- Eval KDL SHA-256: `0935dbe774a13dc1bf99b547c0dfee2df4b6a2124f3cac86ed7297805b54a3a7` (`variants/omp-astra.kdl`)
- Result: `void`

## Timing

- Started: `2026-09-27T08:48:07.552Z`
- Ended: `2026-09-27T08:48:52.466Z`
- Duration: `44.914`

## Model usage

| Role | Harness | Model | Tokens |
| --- | --- | --- | ---: |
| `wake.codex` | `codex` | `gpt-6-sol × 1 turns` | `112,356` |
| `wake.omp` | `omp` | `openai-codex/gpt-6-astra × 7 turns` | `135,130` |
| `wake.omp-2` | `omp` | `openai-codex/gpt-6-astra × 5 turns` | `94,641` |

- Agent tokens: `342,127`
- LLM judge tokens: `0` (no LLM judge)
- Total model tokens: `342,127`
- Count source: provider transcripts. Models are counted per assistant turn in each transcript. Tokens are omp `usage.totalTokens` summed per turn, Codex final `total_token_usage.total_tokens`, and Claude input + output + cache-read + cache-creation tokens summed per turn; cached reads dominate. The mission projection reported `usage: null`.

## Observable graph transitions

| Time | Subject or step | Transition | Evidence |
| --- | --- | --- | --- |
| `08:48:07.552` | `mission-run/cross-omp-hold2-astra-20260927-b` | `absent -> created` | store index 3, mission-run.created |
| `08:48:07.581` | `exercise-message-wake` | `absent -> ready` | store index 9, step-run.state |
| `08:48:51.571` | `exercise-message-wake` | `working -> cancelled` | store index 232, step-run.state |
| `08:48:51.571` | `held-out-gates` | `absent -> cancelled` | store index 233, step-run.state |
| `08:48:51.810` | `cleanup-agents` | `absent -> ready` | store index 238, step-run.state |
| `08:48:52.421` | `cleanup-agents` | `working -> completed` | store index 258, step-run.state |
| `08:48:52.466` | `mission-run/cross-omp-hold2-astra-20260927-b` | `running -> cancelled` | store index 265, mission-run.state |

## Small Talk

- Runtime work messages: `0`
- Direct agent messages: `0`
- Required sequence: per phase: the controller sends 4 kickoffs; each pair exchanges one `FACT`, then one matching `AGREEMENT`; each participant sends one `CONSENSUS` to `person/eval-requester`. The idle phase starts only after all four seats report exactly `idle`
- Unexpected or duplicate messages: the controller never sent kickoffs

## Judges

| Judge | Result | Duration | Evidence |
| --- | --- | ---: | --- |
| none reached | `n/a` | `n/a` | the mission stopped before any gate ran |
| terminal input audit | `pass` | `n/a` | `0` `terminal.input.requested` claims in the whole graph |

## Result details

- Products or commits: none beyond the graph records above
- Cleanup: complete
- Notable behavior: Void run. The run ID made the Claude seat's PTY socket path 105 bytes, one over the kernel limit, so st3 could not start the Claude seat. The runner cancelled the run. `cross-omp-astra-h2-20260927-a` and `-b` repeated it with shorter IDs.
- omp failures: none observed
- Follow-up: none
