# Eval run report — 2026-09-27

- Eval: `cross-harness-message-wake` (fixed Codex gpt-6-sol and Claude opus seats, each paired with an omp seat on `openai-codex/gpt-5.6-luna`)
- Runtime: `st3`
- Run ID: `mission-run/cross-omp-luna-env2-b`
- Candidate commit: st3 binary built from `agent/message-envelope` `f824999a` with `crates/st3/src/boot.rs` restored to `main`: the `<smalltalk-message>` envelope without the new boot-contract sentence (ablation)
- Eval KDL SHA-256: `0459220519955da16f8d62164c8a8c832734796e85d8696ce79ea910d99b1720` (`variants/omp-luna.kdl`)
- Result: `pass`

## Timing

- Started: `2026-09-27T14:09:47.870Z`
- Ended: `2026-09-27T14:12:12.481Z`
- Duration: `144.611`

## Model usage

| Role | Harness | Model | Tokens |
| --- | --- | --- | ---: |
| `wake.claude` | `claude` | `claude-opus-5-5 × 29 turns` | `1,256,501` |
| `wake.codex` | `codex` | `gpt-6-sol × 2 turns` | `320,623` |
| `wake.omp` | `omp` | `openai-codex/gpt-5.6-luna × 26 turns` | `604,156` |
| `wake.omp-2` | `omp` | `openai-codex/gpt-5.6-luna × 25 turns` | `592,863` |

- Agent tokens: `2,774,143`
- LLM judge tokens: `0` (no LLM judge)
- Total model tokens: `2,774,143`
- Count source: provider transcripts. Models are counted per assistant turn in each transcript. Tokens are omp `usage.totalTokens` summed per turn, Codex final `total_token_usage.total_tokens`, and Claude input + output + cache-read + cache-creation tokens summed per turn; cached reads dominate. The mission projection reported `usage: null`.

## Observable graph transitions

| Time | Subject or step | Transition | Evidence |
| --- | --- | --- | --- |
| `14:09:47.870` | `mission-run/cross-omp-luna-env2-b` | `absent -> created` | store index 3, mission-run.created |
| `14:09:47.891` | `exercise-message-wake` | `absent -> ready` | store index 9, step-run.state |
| `14:12:10.299` | `exercise-message-wake` | `working -> completed` | store index 205, step-run.state |
| `14:12:10.328` | `held-out-gates` | `absent -> ready` | store index 206, step-run.state |
| `14:12:11.407` | `held-out-gates` | `working -> completed` | store index 224, step-run.state |
| `14:12:11.461` | `cleanup-agents` | `absent -> ready` | store index 228, step-run.state |
| `14:12:12.459` | `cleanup-agents` | `working -> completed` | store index 250, step-run.state |
| `14:12:12.481` | `mission-run/cross-omp-luna-env2-b` | `running -> completed` | store index 253, mission-run.state |

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
| the canonical conversation proves wake, exchange, and consensus | `pass` | `0.834` | stage `held-out-gates` |
| no terminal input path participated | `pass` | `0.215` | stage `held-out-gates` |
| terminal input audit | `pass` | `n/a` | `0` `terminal.input.requested` claims in the whole graph |

## Result details

- Products or commits: none beyond the graph records above
- Cleanup: complete
- Notable behavior: Startup: kickoffs sent at 14:09:51.873; all receipts +9.4 s; fact stage complete +32.9 s; agreement stage complete +44.1 s; result stage complete +58.5 s.
- Notable behavior: Idle: kickoffs sent at 14:11:30.242; all receipts +4.2 s; fact stage complete +14.4 s; agreement stage complete +31.8 s; result stage complete +40.0 s.
- Notable behavior: Provider transcripts contain 24 `<smalltalk-message id=` and 28 `[PING from st3]` occurrences (the Claude seat's channel notices keep the PING form).
- omp failures: none observed
- Follow-up: none
