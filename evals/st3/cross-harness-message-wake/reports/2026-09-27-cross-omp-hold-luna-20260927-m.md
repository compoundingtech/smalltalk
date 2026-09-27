# Eval run report — 2026-09-27

- Eval: `cross-harness-message-wake` (fixed Codex gpt-6-sol and Claude opus seats, each paired with an omp seat on `openai-codex/gpt-5.6-luna`)
- Runtime: `st3`
- Run ID: `mission-run/cross-omp-hold-luna-20260927-m`
- Candidate commit: st3 binary built from `7e7d8d8`: the first omp mail hold (`9ba4c1b`) and the hook-root fix, on `agent/st3-next` `b138cd3`. This hold waited for omp's idle proof after a run and released held mail as separate steers; `c19aa71` and `c4f80ca` changed both
- Eval KDL SHA-256: `0459220519955da16f8d62164c8a8c832734796e85d8696ce79ea910d99b1720` (`variants/omp-luna.kdl`)
- Result: `fail`

## Timing

- Started: `2026-09-27T08:40:40.625Z`
- Ended: `2026-09-27T08:47:31.882Z`
- Duration: `411.257`

## Model usage

| Role | Harness | Model | Tokens |
| --- | --- | --- | ---: |
| `wake.claude` | `claude` | `claude-opus-5-5 × 32 turns` | `1,398,042` |
| `wake.codex` | `codex` | `gpt-6-sol × 3 turns` | `468,438` |
| `wake.omp` | `omp` | `openai-codex/gpt-5.6-luna × 27 turns` | `633,464` |
| `wake.omp-2` | `omp` | `openai-codex/gpt-5.6-luna × 18 turns` | `437,052` |

- Agent tokens: `2,936,996`
- LLM judge tokens: `0` (no LLM judge)
- Total model tokens: `2,936,996`
- Count source: provider transcripts. Models are counted per assistant turn in each transcript. Tokens are omp `usage.totalTokens` summed per turn, Codex final `total_token_usage.total_tokens`, and Claude input + output + cache-read + cache-creation tokens summed per turn; cached reads dominate. The mission projection reported `usage: null`.

## Observable graph transitions

| Time | Subject or step | Transition | Evidence |
| --- | --- | --- | --- |
| `08:40:40.625` | `mission-run/cross-omp-hold-luna-20260927-m` | `absent -> created` | store index 3, mission-run.created |
| `08:40:40.659` | `exercise-message-wake` | `absent -> ready` | store index 9, step-run.state |
| `08:47:30.867` | `exercise-message-wake` | `working -> cancelled` | store index 207, step-run.state |
| `08:47:30.867` | `held-out-gates` | `absent -> cancelled` | store index 208, step-run.state |
| `08:47:30.894` | `cleanup-agents` | `absent -> ready` | store index 212, step-run.state |
| `08:47:31.861` | `cleanup-agents` | `working -> completed` | store index 234, step-run.state |
| `08:47:31.882` | `mission-run/cross-omp-hold-luna-20260927-m` | `running -> cancelled` | store index 237, mission-run.state |

## Small Talk

- Runtime work messages: `0`
- Direct agent messages: `22`
- Person or controller messages: `8`
- Required sequence: per phase: the controller sends 4 kickoffs; each pair exchanges one `FACT`, then one matching `AGREEMENT`; each participant sends one `CONSENSUS` to `person/eval-requester`. The idle phase starts only after all four seats report exactly `idle`
- Unexpected or duplicate messages: `wake.omp` sent no idle-phase `AGREEMENT`: 8 kickoffs and 22 protocol messages

## Judges

| Judge | Result | Duration | Evidence |
| --- | --- | ---: | --- |
| none reached | `n/a` | `n/a` | the mission stopped before any gate ran |
| terminal input audit | `pass` | `n/a` | `0` `terminal.input.requested` claims in the whole graph |

## Result details

- Products or commits: none beyond the graph records above
- Cleanup: complete
- Notable behavior: Startup: kickoffs sent at 08:40:44.215; receipts +13.5 s, facts +36.1 s, agreements +45.4 s, results +68.3 s. Idle: kickoffs sent at 08:42:16.765; receipts +3.2 s, facts +12.4 s.
- Notable behavior: Both omp `model_change` entries and every omp assistant turn record `openai-codex/gpt-5.6-luna`.
- Notable behavior: With the hold, incoming mail backgrounded omp tool calls: `wake.omp` 0; `wake.omp-2` 3 (including 1 `conversations send` call) (`wake.omp` 30 tool results; `wake.omp-2` 33 tool results). The omp channel handed the 12 messages for omp seats to omp a median 3.2 s after st3 staged them (at most 13.1 s); the omp seats read them a median 9.3 s after they were sent.
- Notable behavior: In the idle phase `wake.omp` sent `FACT ORBIT`, read Codex's `FACT EMBER`, then read Codex's `AGREEMENT EMBER+ORBIT` and sent `CONSENSUS EMBER+ORBIT` without its own agreement. Its final text says it had sent the agreement. The controller exited 1 when the idle agreement stage timed out, and the runner cancelled the run. The agreement reached omp at the batch boundary where an unheld steer would also have landed.
- Notable behavior: Two messages were held through `wake.omp-2`'s first boot batch and steered together when it returned. omp injects one queued steer per boundary, so the second waited through the next model turn and backgrounded all three calls of that turn's batch. `c4f80ca` hands mail released together to omp as one steer.
- Notable behavior: `wake.omp-2` ran `work claim` once and `work complete` twice on the controller's agentless `exercise-message-wake` step; st3 refused each attempt with 422 `work-not-available`.
- omp failures:
  - `wake.omp` skipped its idle-phase `AGREEMENT` to Codex (a model protocol slip)
- Follow-up: none
