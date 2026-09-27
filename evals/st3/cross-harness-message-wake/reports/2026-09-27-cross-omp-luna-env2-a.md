# Eval run report — 2026-09-27

- Eval: `cross-harness-message-wake` (fixed Codex gpt-6-sol and Claude opus seats, each paired with an omp seat on `openai-codex/gpt-5.6-luna`)
- Runtime: `st3`
- Run ID: `mission-run/cross-omp-luna-env2-a`
- Candidate commit: st3 binary built from `agent/message-envelope` `f824999a` with `crates/st3/src/boot.rs` restored to `main`: the `<smalltalk-message>` envelope without the new boot-contract sentence (ablation)
- Eval KDL SHA-256: `0459220519955da16f8d62164c8a8c832734796e85d8696ce79ea910d99b1720` (`variants/omp-luna.kdl`)
- Result: `pass`

## Timing

- Started: `2026-09-27T14:04:10.251Z`
- Ended: `2026-09-27T14:05:55.755Z`
- Duration: `105.504`

## Model usage

| Role | Harness | Model | Tokens |
| --- | --- | --- | ---: |
| `wake.claude` | `claude` | `claude-opus-5-5 × 34 turns` | `1,467,232` |
| `wake.codex` | `codex` | `gpt-6-sol × 3 turns` | `428,821` |
| `wake.omp` | `omp` | `openai-codex/gpt-5.6-luna × 19 turns` | `404,319` |
| `wake.omp-2` | `omp` | `openai-codex/gpt-5.6-luna × 17 turns` | `357,415` |

- Agent tokens: `2,657,787`
- LLM judge tokens: `0` (no LLM judge)
- Total model tokens: `2,657,787`
- Count source: provider transcripts. Models are counted per assistant turn in each transcript. Tokens are omp `usage.totalTokens` summed per turn, Codex final `total_token_usage.total_tokens`, and Claude input + output + cache-read + cache-creation tokens summed per turn; cached reads dominate. The mission projection reported `usage: null`.

## Observable graph transitions

| Time | Subject or step | Transition | Evidence |
| --- | --- | --- | --- |
| `14:04:10.251` | `mission-run/cross-omp-luna-env2-a` | `absent -> created` | store index 3, mission-run.created |
| `14:04:10.273` | `exercise-message-wake` | `absent -> ready` | store index 9, step-run.state |
| `14:05:53.463` | `exercise-message-wake` | `working -> completed` | store index 206, step-run.state |
| `14:05:53.494` | `held-out-gates` | `absent -> ready` | store index 207, step-run.state |
| `14:05:54.675` | `held-out-gates` | `working -> completed` | store index 226, step-run.state |
| `14:05:54.728` | `cleanup-agents` | `absent -> ready` | store index 229, step-run.state |
| `14:05:55.733` | `cleanup-agents` | `working -> completed` | store index 251, step-run.state |
| `14:05:55.755` | `mission-run/cross-omp-luna-env2-a` | `running -> completed` | store index 254, mission-run.state |

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
| the canonical conversation proves wake, exchange, and consensus | `pass` | `0.904` | stage `held-out-gates` |
| no terminal input path participated | `pass` | `0.244` | stage `held-out-gates` |
| terminal input audit | `pass` | `n/a` | `0` `terminal.input.requested` claims in the whole graph |

## Result details

- Products or commits: none beyond the graph records above
- Cleanup: complete
- Notable behavior: Startup: kickoffs sent at 14:04:15.115; all receipts +7.3 s; fact stage complete +24.0 s; agreement stage complete +34.3 s; result stage complete +48.5 s.
- Notable behavior: Idle: kickoffs sent at 14:05:19.432; all receipts +4.2 s; fact stage complete +13.5 s; agreement stage complete +22.7 s; result stage complete +34.0 s.
- Notable behavior: Provider transcripts contain 24 `<smalltalk-message id=` and 25 `[PING from st3]` occurrences (the Claude seat's channel notices keep the PING form).
- omp failures: none observed
- Follow-up: none
