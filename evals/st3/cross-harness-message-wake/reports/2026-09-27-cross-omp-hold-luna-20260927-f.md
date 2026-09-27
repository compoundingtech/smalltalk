# Eval run report — 2026-09-27

- Eval: `cross-harness-message-wake` (fixed Codex gpt-6-sol and Claude opus seats, each paired with an omp seat on `openai-codex/gpt-5.6-luna`)
- Runtime: `st3`
- Run ID: `mission-run/cross-omp-hold-luna-20260927-f`
- Candidate commit: st3 binary built from `7e7d8d8`: the first omp mail hold (`9ba4c1b`) and the hook-root fix, on `agent/st3-next` `b138cd3`. This hold waited for omp's idle proof after a run and released held mail as separate steers; `c19aa71` and `c4f80ca` changed both
- Eval KDL SHA-256: `0459220519955da16f8d62164c8a8c832734796e85d8696ce79ea910d99b1720` (`variants/omp-luna.kdl`)
- Result: `pass`

## Timing

- Started: `2026-09-27T08:36:10.560Z`
- Ended: `2026-09-27T08:38:11.782Z`
- Duration: `121.222`

## Model usage

| Role | Harness | Model | Tokens |
| --- | --- | --- | ---: |
| `wake.claude` | `claude` | `claude-opus-5-5 × 32 turns` | `1,427,834` |
| `wake.codex` | `codex` | `gpt-6-sol × 2 turns` | `482,273` |
| `wake.omp` | `omp` | `openai-codex/gpt-5.6-luna × 18 turns` | `392,125` |
| `wake.omp-2` | `omp` | `openai-codex/gpt-5.6-luna × 20 turns` | `448,789` |

- Agent tokens: `2,751,021`
- LLM judge tokens: `0` (no LLM judge)
- Total model tokens: `2,751,021`
- Count source: provider transcripts. Models are counted per assistant turn in each transcript. Tokens are omp `usage.totalTokens` summed per turn, Codex final `total_token_usage.total_tokens`, and Claude input + output + cache-read + cache-creation tokens summed per turn; cached reads dominate. The mission projection reported `usage: null`.

## Observable graph transitions

| Time | Subject or step | Transition | Evidence |
| --- | --- | --- | --- |
| `08:36:10.560` | `mission-run/cross-omp-hold-luna-20260927-f` | `absent -> created` | store index 3, mission-run.created |
| `08:36:10.583` | `exercise-message-wake` | `absent -> ready` | store index 9, step-run.state |
| `08:38:09.032` | `exercise-message-wake` | `working -> completed` | store index 212, step-run.state |
| `08:38:09.070` | `held-out-gates` | `absent -> ready` | store index 213, step-run.state |
| `08:38:10.572` | `held-out-gates` | `working -> completed` | store index 232, step-run.state |
| `08:38:10.637` | `cleanup-agents` | `absent -> ready` | store index 235, step-run.state |
| `08:38:11.758` | `cleanup-agents` | `working -> completed` | store index 257, step-run.state |
| `08:38:11.782` | `mission-run/cross-omp-hold-luna-20260927-f` | `running -> completed` | store index 260, mission-run.state |

## Small Talk

- Runtime work messages: `0`
- Direct agent messages: `24`
- Person or controller messages: `8`
- Required sequence: per phase: the controller sends 4 kickoffs; each pair exchanges one `FACT`, then one matching `AGREEMENT`; each participant sends one `CONSENSUS` to `person/eval-requester`. The idle phase starts only after all four seats report exactly `idle`
- Unexpected or duplicate messages: none: 8 kickoffs and exactly 24 protocol messages, each sent by its real seat

## Judges

| Judge | Result | Duration | Evidence |
| --- | --- | ---: | --- |
| the controller observed both final reports | `pass` | `n/a` | stage `exercise-message-wake` |
| the canonical conversation proves wake, exchange, and consensus | `pass` | `1.124` | stage `held-out-gates` |
| no terminal input path participated | `pass` | `0.334` | stage `held-out-gates` |
| terminal input audit | `pass` | `n/a` | `0` `terminal.input.requested` claims in the whole graph |

## Result details

- Products or commits: none beyond the graph records above
- Cleanup: complete
- Notable behavior: Startup: kickoffs sent at 08:36:14.677; receipts +8.4 s, facts +54.1 s, agreements +65.4 s, results +76.7 s. Idle: kickoffs sent at 08:37:37.757; receipts +4.2 s, facts +11.4 s, agreements +21.7 s, results +31.2 s.
- Notable behavior: Both omp `model_change` entries and every omp assistant turn record `openai-codex/gpt-5.6-luna`.
- Notable behavior: With the hold, no omp tool call was backgrounded by incoming mail (`wake.omp` 14 tool results; `wake.omp-2` 37 tool results). The omp channel handed the 12 messages for omp seats to omp a median 1.9 s after st3 staged them (at most 6.9 s); the omp seats read them a median 8.0 s after they were sent.
- Notable behavior: `wake.omp-2` ran `work complete` once and `work claim` twice on the controller's agentless `exercise-message-wake` step; st3 refused each attempt with 422 `work-not-available`.
- omp failures: none observed
- Follow-up: none
