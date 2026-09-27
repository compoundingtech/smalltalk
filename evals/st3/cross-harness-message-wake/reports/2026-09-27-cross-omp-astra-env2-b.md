# Eval run report — 2026-09-27

- Eval: `cross-harness-message-wake` (fixed Codex gpt-6-sol and Claude opus seats, each paired with an omp seat on `openai-codex/gpt-6-astra`)
- Runtime: `st3`
- Run ID: `mission-run/cross-omp-astra-env2-b`
- Candidate commit: st3 binary built from `agent/message-envelope` `f824999a` with `crates/st3/src/boot.rs` restored to `main`: the `<smalltalk-message>` envelope without the new boot-contract sentence (ablation)
- Eval KDL SHA-256: `0935dbe774a13dc1bf99b547c0dfee2df4b6a2124f3cac86ed7297805b54a3a7` (`variants/omp-astra.kdl`)
- Result: `pass`

## Timing

- Started: `2026-09-27T14:09:51.973Z`
- Ended: `2026-09-27T14:11:40.398Z`
- Duration: `108.425`

## Model usage

| Role | Harness | Model | Tokens |
| --- | --- | --- | ---: |
| `wake.claude` | `claude` | `claude-opus-5-5 × 29 turns` | `1,288,368` |
| `wake.codex` | `codex` | `gpt-6-sol × 5 turns` | `354,402` |
| `wake.omp` | `omp` | `openai-codex/gpt-6-astra × 17 turns` | `343,560` |
| `wake.omp-2` | `omp` | `openai-codex/gpt-6-astra × 16 turns` | `343,364` |

- Agent tokens: `2,329,694`
- LLM judge tokens: `0` (no LLM judge)
- Total model tokens: `2,329,694`
- Count source: provider transcripts. Models are counted per assistant turn in each transcript. Tokens are omp `usage.totalTokens` summed per turn, Codex final `total_token_usage.total_tokens`, and Claude input + output + cache-read + cache-creation tokens summed per turn; cached reads dominate. The mission projection reported `usage: null`.

## Observable graph transitions

| Time | Subject or step | Transition | Evidence |
| --- | --- | --- | --- |
| `14:09:51.973` | `mission-run/cross-omp-astra-env2-b` | `absent -> created` | store index 3, mission-run.created |
| `14:09:51.994` | `exercise-message-wake` | `absent -> ready` | store index 9, step-run.state |
| `14:11:38.047` | `exercise-message-wake` | `working -> completed` | store index 211, step-run.state |
| `14:11:38.074` | `held-out-gates` | `absent -> ready` | store index 212, step-run.state |
| `14:11:39.322` | `held-out-gates` | `working -> completed` | store index 230, step-run.state |
| `14:11:39.377` | `cleanup-agents` | `absent -> ready` | store index 233, step-run.state |
| `14:11:40.376` | `cleanup-agents` | `working -> completed` | store index 255, step-run.state |
| `14:11:40.398` | `mission-run/cross-omp-astra-env2-b` | `running -> completed` | store index 258, mission-run.state |

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
| the canonical conversation proves wake, exchange, and consensus | `pass` | `0.968` | stage `held-out-gates` |
| no terminal input path participated | `pass` | `0.249` | stage `held-out-gates` |
| terminal input audit | `pass` | `n/a` | `0` `terminal.input.requested` claims in the whole graph |

## Result details

- Products or commits: none beyond the graph records above
- Cleanup: complete
- Notable behavior: Startup: kickoffs sent at 14:09:55.481; all receipts +5.3 s; fact stage complete +26.8 s; agreement stage complete +38.1 s; result stage complete +48.4 s.
- Notable behavior: Idle: kickoffs sent at 14:10:52.585; all receipts +4.2 s; fact stage complete +18.7 s; agreement stage complete +33.1 s; result stage complete +45.4 s.
- Notable behavior: Provider transcripts contain 24 `<smalltalk-message id=` and 24 `[PING from st3]` occurrences (the Claude seat's channel notices keep the PING form).
- omp failures: none observed
- Follow-up: none
