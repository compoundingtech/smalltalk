# Eval run report — 2026-09-26

- Eval: `cross-harness-message-wake` (fixed Codex gpt-6-sol and Claude opus seats, each paired with a Claude `claude-sonnet-5` seat)
- Runtime: `st3`
- Run ID: `mission-run/cross-sonnet-final-20260926-b`
- Candidate commit: st3 binary built from `72c04ce`, the branch's final st3 source
- Eval KDL SHA-256: `611ec4fcd987730d2cc6c95ad3d38b34cb9a997c4fac88997d3a2d4bee7bc376` (`variants/claude-sonnet.kdl`)
- Result: `stopped`

## Timing

- Started: `2026-09-26T23:24:02.097Z`
- Ended: `2026-09-26T23:29:20.018Z`
- Duration: `317.921`

## Model usage

| Role | Harness | Model | Tokens |
| --- | --- | --- | ---: |
| `unmapped` | `claude` | `claude-sonnet-5 × 6 turns` | `225,258` |
| `unmapped` | `claude` | `claude-opus-5-5 × 7 turns` | `268,997` |
| `unmapped` | `claude` | `claude-sonnet-5 × 4 turns` | `149,228` |
| `unmapped` | `codex` | `gpt-6-sol × 1 turns` | `89,967` |

- Agent tokens: `733,450`
- LLM judge tokens: `0` (no LLM judge)
- Total model tokens: `733,450`
- Count source: provider transcripts. Models are counted per assistant turn in each transcript. Tokens are omp `usage.totalTokens` summed per turn, Codex final `total_token_usage.total_tokens`, and Claude input + output + cache-read + cache-creation tokens summed per turn; cached reads dominate. The mission projection reported `usage: null`.

## Observable graph transitions

| Time | Subject or step | Transition | Evidence |
| --- | --- | --- | --- |
| `23:24:02.097` | `mission-run/cross-sonnet-final-20260926-b` | `absent -> created` | store index 3, mission-run.created |
| `23:24:02.124` | `exercise-message-wake` | `absent -> ready` | store index 9, step-run.state |
| `23:29:18.355` | `exercise-message-wake` | `working -> cancelled` | store index 57, step-run.state |
| `23:29:18.356` | `held-out-gates` | `absent -> cancelled` | store index 58, step-run.state |
| `23:29:18.386` | `cleanup-agents` | `absent -> ready` | store index 62, step-run.state |
| `23:29:19.996` | `cleanup-agents` | `working -> completed` | store index 82, step-run.state |
| `23:29:20.018` | `mission-run/cross-sonnet-final-20260926-b` | `running -> cancelled` | store index 85, mission-run.state |

## Small Talk

- Runtime work messages: `0`
- Direct agent messages: `0`
- Required sequence: per phase: the controller sends 4 kickoffs; each pair exchanges one `FACT`, then one matching `AGREEMENT`; each participant sends one `CONSENSUS` to `person/eval-requester`
- Unexpected or duplicate messages: none; the protocol never started

## Judges

| Judge | Result | Duration | Evidence |
| --- | --- | ---: | --- |
| none reached | `n/a` | `n/a` | the mission stopped before any gate ran |
| terminal input audit | `pass` | `n/a` | `0` `terminal.input.requested` claims in the whole graph |

## Result details

- Products or commits: none beyond the graph records above
- Cleanup: complete
- Notable behavior: Eval host fault, not a harness or st3 result: the eval host's RAM-backed `/tmp` filled while this run launched, because every earlier run kept a copy of the debug st3 binary. The Codex seat never started and stayed `desired`, so the run was cancelled at 23:29 and rerun as the `-c` run. The runner now removes its binary copy at teardown.
- Follow-up: rerun as the `-c` run
