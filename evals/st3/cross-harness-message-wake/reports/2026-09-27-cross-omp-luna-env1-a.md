# Eval run report — 2026-09-27

- Eval: `cross-harness-message-wake` (fixed Codex gpt-6-sol and Claude opus seats, each paired with an omp seat on `openai-codex/gpt-5.6-luna`)
- Runtime: `st3`
- Run ID: `mission-run/cross-omp-luna-env1-a`
- Candidate commit: st3 binary built from `agent/message-envelope` `f824999a`: the `<smalltalk-message>` envelope for Codex, OpenCode, pi and omp and the new boot-contract sentence
- Eval KDL SHA-256: `0459220519955da16f8d62164c8a8c832734796e85d8696ce79ea910d99b1720` (`variants/omp-luna.kdl`)
- Result: `pass`

## Timing

- Started: `2026-09-27T13:06:18.875Z`
- Ended: `2026-09-27T13:08:16.729Z`
- Duration: `117.854`

## Model usage

| Role | Harness | Model | Tokens |
| --- | --- | --- | ---: |
| `wake.claude` | `claude` | `claude-opus-5-5 × 31 turns` | `1,370,233` |
| `wake.codex` | `codex` | `gpt-6-sol × 3 turns` | `399,866` |
| `wake.omp` | `omp` | `openai-codex/gpt-5.6-luna × 24 turns` | `545,207` |
| `wake.omp-2` | `omp` | `openai-codex/gpt-5.6-luna × 28 turns` | `648,423` |

- Agent tokens: `2,963,729`
- LLM judge tokens: `0` (no LLM judge)
- Total model tokens: `2,963,729`
- Count source: provider transcripts. Models are counted per assistant turn in each transcript. Tokens are omp `usage.totalTokens` summed per turn, Codex final `total_token_usage.total_tokens`, and Claude input + output + cache-read + cache-creation tokens summed per turn; cached reads dominate. The mission projection reported `usage: null`.

## Observable graph transitions

| Time | Subject or step | Transition | Evidence |
| --- | --- | --- | --- |
| `13:06:18.875` | `mission-run/cross-omp-luna-env1-a` | `absent -> created` | store index 3, mission-run.created |
| `13:06:18.895` | `exercise-message-wake` | `absent -> ready` | store index 9, step-run.state |
| `13:08:14.414` | `exercise-message-wake` | `working -> completed` | store index 207, step-run.state |
| `13:08:14.441` | `held-out-gates` | `absent -> ready` | store index 208, step-run.state |
| `13:08:15.506` | `held-out-gates` | `working -> completed` | store index 226, step-run.state |
| `13:08:15.559` | `cleanup-agents` | `absent -> ready` | store index 229, step-run.state |
| `13:08:16.707` | `cleanup-agents` | `working -> completed` | store index 251, step-run.state |
| `13:08:16.729` | `mission-run/cross-omp-luna-env1-a` | `running -> completed` | store index 254, mission-run.state |

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
| the canonical conversation proves wake, exchange, and consensus | `pass` | `0.819` | stage `held-out-gates` |
| no terminal input path participated | `pass` | `0.216` | stage `held-out-gates` |
| terminal input audit | `pass` | `n/a` | `0` `terminal.input.requested` claims in the whole graph |

## Result details

- Products or commits: none beyond the graph records above
- Cleanup: complete
- Notable behavior: Startup: kickoffs sent at 13:06:22.307; all receipts +8.3 s; fact stage complete +34.5 s; agreement stage complete +42.8 s; result stage complete +52.1 s.
- Notable behavior: Idle: kickoffs sent at 13:07:42.567; all receipts +3.2 s; fact stage complete +17.4 s; agreement stage complete +24.6 s; result stage complete +31.8 s.
- Notable behavior: Provider transcripts contain 24 `<smalltalk-message id=` and 23 `[PING from st3]` occurrences (the Claude seat's channel notices keep the PING form).
- omp failures: none observed
- Follow-up: none
