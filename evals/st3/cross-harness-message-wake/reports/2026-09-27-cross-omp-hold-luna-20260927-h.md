# Eval run report — 2026-09-27

- Eval: `cross-harness-message-wake` (fixed Codex gpt-6-sol and Claude opus seats, each paired with an omp seat on `openai-codex/gpt-5.6-luna`)
- Runtime: `st3`
- Run ID: `mission-run/cross-omp-hold-luna-20260927-h`
- Candidate commit: st3 binary built from `7e7d8d8`: the first omp mail hold (`9ba4c1b`) and the hook-root fix, on `agent/st3-next` `b138cd3`. This hold waited for omp's idle proof after a run and released held mail as separate steers; `c19aa71` and `c4f80ca` changed both
- Eval KDL SHA-256: `0459220519955da16f8d62164c8a8c832734796e85d8696ce79ea910d99b1720` (`variants/omp-luna.kdl`)
- Result: `fail`

## Timing

- Started: `2026-09-27T08:36:16.669Z`
- Ended: `2026-09-27T08:41:34.893Z`
- Duration: `318.224`

## Model usage

| Role | Harness | Model | Tokens |
| --- | --- | --- | ---: |
| `wake.claude` | `claude` | `claude-opus-5-5 × 20 turns` | `823,289` |
| `wake.codex` | `codex` | `gpt-6-sol × 1 turns` | `111,833` |
| `wake.omp` | `omp` | `openai-codex/gpt-5.6-luna × 5 turns` | `99,284` |
| `wake.omp-2` | `omp` | `openai-codex/gpt-5.6-luna × 10 turns` | `213,128` |

- Agent tokens: `1,247,534`
- LLM judge tokens: `0` (no LLM judge)
- Total model tokens: `1,247,534`
- Count source: provider transcripts. Models are counted per assistant turn in each transcript. Tokens are omp `usage.totalTokens` summed per turn, Codex final `total_token_usage.total_tokens`, and Claude input + output + cache-read + cache-creation tokens summed per turn; cached reads dominate. The mission projection reported `usage: null`.

## Observable graph transitions

| Time | Subject or step | Transition | Evidence |
| --- | --- | --- | --- |
| `08:36:16.669` | `mission-run/cross-omp-hold-luna-20260927-h` | `absent -> created` | store index 3, mission-run.created |
| `08:36:16.692` | `exercise-message-wake` | `absent -> ready` | store index 9, step-run.state |
| `08:41:33.751` | `exercise-message-wake` | `working -> cancelled` | store index 112, step-run.state |
| `08:41:33.751` | `held-out-gates` | `absent -> cancelled` | store index 113, step-run.state |
| `08:41:33.778` | `cleanup-agents` | `absent -> ready` | store index 117, step-run.state |
| `08:41:34.869` | `cleanup-agents` | `working -> completed` | store index 139, step-run.state |
| `08:41:34.893` | `mission-run/cross-omp-hold-luna-20260927-h` | `running -> cancelled` | store index 142, mission-run.state |

## Small Talk

- Runtime work messages: `0`
- Direct agent messages: `7`
- Person or controller messages: `4`
- Required sequence: per phase: the controller sends 4 kickoffs; each pair exchanges one `FACT`, then one matching `AGREEMENT`; each participant sends one `CONSENSUS` to `person/eval-requester`. The idle phase starts only after all four seats report exactly `idle`
- Unexpected or duplicate messages: the fixed Codex seat sent no protocol message; 4 kickoffs and 7 protocol messages

## Judges

| Judge | Result | Duration | Evidence |
| --- | --- | ---: | --- |
| none reached | `n/a` | `n/a` | the mission stopped before any gate ran |
| terminal input audit | `pass` | `n/a` | `0` `terminal.input.requested` claims in the whole graph |

## Result details

- Products or commits: none beyond the graph records above
- Cleanup: complete
- Notable behavior: Startup: kickoffs sent at 08:36:20.317; receipts +8.3 s.
- Notable behavior: Both omp `model_change` entries and every omp assistant turn record `openai-codex/gpt-5.6-luna`.
- Notable behavior: With the hold, no omp tool call was backgrounded by incoming mail (`wake.omp` 5 tool results; `wake.omp-2` 18 tool results). The omp channel handed the 4 messages for omp seats to omp a median 3.3 s after st3 staged them (at most 7.0 s); the omp seats read them a median 11.2 s after they were sent.
- Notable behavior: The fixed Codex gpt-6-sol seat read its kickoff and the omp seat's `FACT ORBIT` but sent nothing. Its last message says it stopped because no step was available to claim. The omp-2/Claude pair completed its startup exchange, and `wake.omp` sent its fact and waited. The controller exited 1 when the startup fact stage timed out after 300 s, and the runner cancelled the run. This is the Codex boot-contract stop recorded in `cross-omp-head-20260926-b`, not an omp failure.
- omp failures: none observed
- Follow-up: none
