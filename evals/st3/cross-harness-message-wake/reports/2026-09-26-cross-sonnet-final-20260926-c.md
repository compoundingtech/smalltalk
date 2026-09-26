# Eval run report — 2026-09-26

- Eval: `cross-harness-message-wake` (fixed Codex gpt-6-sol and Claude opus seats, each paired with a Claude `claude-sonnet-5` seat, final candidate)
- Runtime: `st3`
- Run ID: `mission-run/cross-sonnet-final-20260926-c`
- Candidate commit: st3 binary built from `72c04ce`, the branch's final st3 source; eval files as committed in `22158cd`
- Eval KDL SHA-256: `611ec4fcd987730d2cc6c95ad3d38b34cb9a997c4fac88997d3a2d4bee7bc376` (`variants/claude-sonnet.kdl`)
- Result: `pass`

## Timing

- Started: `2026-09-26T23:29:30.235Z`
- Ended: `2026-09-26T23:30:52.605Z`
- Duration: `82.370`

## Model usage

| Role | Harness | Model | Tokens |
| --- | --- | --- | ---: |
| `wake.claude` | `claude` | `claude-opus-5-5 × 29 turns` | `1,263,273` |
| `wake.codex` | `codex` | `gpt-6-sol × 2 turns` | `356,858` |
| `wake.sonnet` | `claude` | `claude-sonnet-5 × 27 turns` | `1,119,172` |
| `wake.sonnet-2` | `claude` | `claude-sonnet-5 × 23 turns` | `962,104` |

- Agent tokens: `3,701,407`
- LLM judge tokens: `0` (no LLM judge)
- Total model tokens: `3,701,407`
- Count source: provider transcripts. Models are counted per assistant turn in each transcript. Tokens are omp `usage.totalTokens` summed per turn, Codex final `total_token_usage.total_tokens`, and Claude input + output + cache-read + cache-creation tokens summed per turn; cached reads dominate. The mission projection reported `usage: null`.

## Observable graph transitions

| Time | Subject or step | Transition | Evidence |
| --- | --- | --- | --- |
| `23:29:30.235` | `mission-run/cross-sonnet-final-20260926-c` | `absent -> created` | store index 3, mission-run.created |
| `23:29:30.265` | `exercise-message-wake` | `absent -> ready` | store index 9, step-run.state |
| `23:30:49.903` | `exercise-message-wake` | `working -> completed` | store index 220, step-run.state |
| `23:30:49.936` | `held-out-gates` | `absent -> ready` | store index 221, step-run.state |
| `23:30:50.956` | `held-out-gates` | `working -> completed` | store index 239, step-run.state |
| `23:30:51.017` | `cleanup-agents` | `absent -> ready` | store index 242, step-run.state |
| `23:30:52.580` | `cleanup-agents` | `working -> completed` | store index 264, step-run.state |
| `23:30:52.605` | `mission-run/cross-sonnet-final-20260926-c` | `running -> completed` | store index 267, mission-run.state |

## Small Talk

- Runtime work messages: `0`
- Direct agent messages: `24`
- Person or controller messages: `8`
- Required sequence: per phase: the controller sends 4 kickoffs; each pair exchanges one `FACT`, then one matching `AGREEMENT`; each participant sends one `CONSENSUS` to `person/eval-requester`. The idle phase starts only after all four seats report exactly `idle`
- Unexpected or duplicate messages: none: 8 kickoffs and exactly 24 protocol messages

## Judges

| Judge | Result | Duration | Evidence |
| --- | --- | ---: | --- |
| the controller observed both final reports | `pass` | `n/a` | stage `exercise-message-wake` |
| the canonical conversation proves wake, exchange, and consensus | `pass` | `0.000` | stage `held-out-gates` |
| no terminal input path participated | `pass` | `0.227` | stage `held-out-gates` |
| terminal input audit | `pass` | `n/a` | `0` `terminal.input.requested` claims in the whole graph |

## Result details

- Products or commits: none beyond the graph records above
- Cleanup: complete
- Notable behavior: Startup: kickoffs sent at 23:29:33.764; receipts +4.2 s, facts +29.6 s, agreements +32.8 s, results +45.0 s. Idle: kickoffs sent at 23:30:25.205; receipts +4.2 s, facts +11.3 s, agreements +15.5 s, results +24.7 s.
- Follow-up: none
