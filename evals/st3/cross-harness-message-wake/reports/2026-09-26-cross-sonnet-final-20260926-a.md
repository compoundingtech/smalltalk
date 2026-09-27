# Eval run report — 2026-09-26

- Eval: `cross-harness-message-wake` (fixed Codex gpt-6-sol and Claude opus seats, each paired with a Claude `claude-sonnet-5` seat)
- Runtime: `st3`
- Run ID: `mission-run/cross-sonnet-final-20260926-a`
- Candidate commit: st3 binary built from `f6c76b6`, which also listed boot-contract work with `--as "$ST_AGENT"`; `72c04ce` reverted that listing
- Eval KDL SHA-256: `611ec4fcd987730d2cc6c95ad3d38b34cb9a997c4fac88997d3a2d4bee7bc376` (`variants/claude-sonnet.kdl`)
- Result: `pass`

## Timing

- Started: `2026-09-26T23:12:16.157Z`
- Ended: `2026-09-26T23:13:39.279Z`
- Duration: `83.122`

## Model usage

| Role | Harness | Model | Tokens |
| --- | --- | --- | ---: |
| `wake.claude` | `claude` | `claude-opus-5-5 × 21 turns` | `895,631` |
| `wake.codex` | `codex` | `gpt-6-sol × 2 turns` | `363,773` |
| `wake.sonnet` | `claude` | `claude-sonnet-5 × 17 turns` | `687,513` |
| `wake.sonnet-2` | `claude` | `claude-sonnet-5 × 17 turns` | `686,618` |

- Agent tokens: `2,633,535`
- LLM judge tokens: `0` (no LLM judge)
- Total model tokens: `2,633,535`
- Count source: provider transcripts. Models are counted per assistant turn in each transcript. Tokens are omp `usage.totalTokens` summed per turn, Codex final `total_token_usage.total_tokens`, and Claude input + output + cache-read + cache-creation tokens summed per turn; cached reads dominate. The mission projection reported `usage: null`.

## Observable graph transitions

| Time | Subject or step | Transition | Evidence |
| --- | --- | --- | --- |
| `23:12:16.157` | `mission-run/cross-sonnet-final-20260926-a` | `absent -> created` | store index 3, mission-run.created |
| `23:12:16.182` | `exercise-message-wake` | `absent -> ready` | store index 9, step-run.state |
| `23:13:36.605` | `exercise-message-wake` | `working -> completed` | store index 215, step-run.state |
| `23:13:36.636` | `held-out-gates` | `absent -> ready` | store index 216, step-run.state |
| `23:13:37.689` | `held-out-gates` | `working -> completed` | store index 234, step-run.state |
| `23:13:37.750` | `cleanup-agents` | `absent -> ready` | store index 237, step-run.state |
| `23:13:39.257` | `cleanup-agents` | `working -> completed` | store index 259, step-run.state |
| `23:13:39.279` | `mission-run/cross-sonnet-final-20260926-a` | `running -> completed` | store index 262, mission-run.state |

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
| no terminal input path participated | `pass` | `0.234` | stage `held-out-gates` |
| terminal input audit | `pass` | `n/a` | `0` `terminal.input.requested` claims in the whole graph |

## Result details

- Products or commits: none beyond the graph records above
- Cleanup: complete
- Notable behavior: Both phases passed on this intermediate candidate, although its boot contract hid the controller step from `work ls`; Codex gpt-6-sol still ran the protocol here.
- Follow-up: none
