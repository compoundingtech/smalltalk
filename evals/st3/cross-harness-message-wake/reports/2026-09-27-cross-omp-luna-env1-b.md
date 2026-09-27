# Eval run report — 2026-09-27

- Eval: `cross-harness-message-wake` (fixed Codex gpt-6-sol and Claude opus seats, each paired with an omp seat on `openai-codex/gpt-5.6-luna`)
- Runtime: `st3`
- Run ID: `mission-run/cross-omp-luna-env1-b`
- Candidate commit: st3 binary built from `agent/message-envelope` `f824999a`: the `<smalltalk-message>` envelope for Codex, OpenCode, pi and omp and the new boot-contract sentence
- Eval KDL SHA-256: `0459220519955da16f8d62164c8a8c832734796e85d8696ce79ea910d99b1720` (`variants/omp-luna.kdl`)
- Result: `pass`

## Timing

- Started: `2026-09-27T13:42:03.094Z`
- Ended: `2026-09-27T13:44:03.252Z`
- Duration: `120.158`

## Model usage

| Role | Harness | Model | Tokens |
| --- | --- | --- | ---: |
| `wake.claude` | `claude` | `claude-opus-5-5 × 34 turns` | `1,484,590` |
| `wake.codex` | `codex` | `gpt-6-sol × 3 turns` | `457,836` |
| `wake.omp` | `omp` | `openai-codex/gpt-5.6-luna × 22 turns` | `453,390` |
| `wake.omp-2` | `omp` | `openai-codex/gpt-5.6-luna × 18 turns` | `407,626` |

- Agent tokens: `2,803,442`
- LLM judge tokens: `0` (no LLM judge)
- Total model tokens: `2,803,442`
- Count source: provider transcripts. Models are counted per assistant turn in each transcript. Tokens are omp `usage.totalTokens` summed per turn, Codex final `total_token_usage.total_tokens`, and Claude input + output + cache-read + cache-creation tokens summed per turn; cached reads dominate. The mission projection reported `usage: null`.

## Observable graph transitions

| Time | Subject or step | Transition | Evidence |
| --- | --- | --- | --- |
| `13:42:03.094` | `mission-run/cross-omp-luna-env1-b` | `absent -> created` | store index 3, mission-run.created |
| `13:42:03.114` | `exercise-message-wake` | `absent -> ready` | store index 9, step-run.state |
| `13:44:00.662` | `exercise-message-wake` | `working -> completed` | store index 211, step-run.state |
| `13:44:00.692` | `held-out-gates` | `absent -> ready` | store index 212, step-run.state |
| `13:44:02.008` | `held-out-gates` | `working -> completed` | store index 230, step-run.state |
| `13:44:02.066` | `cleanup-agents` | `absent -> ready` | store index 233, step-run.state |
| `13:44:03.227` | `cleanup-agents` | `working -> completed` | store index 255, step-run.state |
| `13:44:03.252` | `mission-run/cross-omp-luna-env1-b` | `running -> completed` | store index 258, mission-run.state |

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
| the canonical conversation proves wake, exchange, and consensus | `pass` | `1.020` | stage `held-out-gates` |
| no terminal input path participated | `pass` | `0.264` | stage `held-out-gates` |
| terminal input audit | `pass` | `n/a` | `0` `terminal.input.requested` claims in the whole graph |

## Result details

- Products or commits: none beyond the graph records above
- Cleanup: complete
- Notable behavior: Startup: kickoffs sent at 13:42:06.548; all receipts +5.2 s; fact stage complete +53.3 s; agreement stage complete +64.6 s; result stage complete +75.9 s.
- Notable behavior: Idle: kickoffs sent at 13:43:30.058; all receipts +3.1 s; fact stage complete +10.7 s; agreement stage complete +21.3 s; result stage complete +30.6 s.
- Notable behavior: Provider transcripts contain 24 `<smalltalk-message id=` and 27 `[PING from st3]` occurrences (the Claude seat's channel notices keep the PING form).
- omp failures: none observed
- Follow-up: none
