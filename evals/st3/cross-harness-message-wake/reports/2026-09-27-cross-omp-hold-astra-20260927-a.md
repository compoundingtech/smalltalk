# Eval run report — 2026-09-27

- Eval: `cross-harness-message-wake` (fixed Codex gpt-6-sol and Claude opus seats, each paired with an omp seat on `openai-codex/gpt-6-astra`)
- Runtime: `st3`
- Run ID: `mission-run/cross-omp-hold-astra-20260927-a`
- Candidate commit: st3 binary built from `7e7d8d8`: the first omp mail hold (`9ba4c1b`) and the hook-root fix, on `agent/st3-next` `b138cd3`. This hold waited for omp's idle proof after a run and released held mail as separate steers; `c19aa71` and `c4f80ca` changed both
- Eval KDL SHA-256: `0935dbe774a13dc1bf99b547c0dfee2df4b6a2124f3cac86ed7297805b54a3a7` (`variants/omp-astra.kdl`)
- Result: `fail`

## Timing

- Started: `2026-09-27T08:38:03.918Z`
- Ended: `2026-09-27T08:49:55.977Z`
- Duration: `712.059`

## Model usage

| Role | Harness | Model | Tokens |
| --- | --- | --- | ---: |
| `wake.claude` | `claude` | `claude-opus-5-5 × 23 turns` | `961,828` |
| `wake.codex` | `codex` | `gpt-6-sol × 1 turns` | `229,116` |
| `wake.omp` | `omp` | `openai-codex/gpt-6-astra × 10 turns` | `201,236` |
| `wake.omp-2` | `omp` | `openai-codex/gpt-6-astra × 9 turns` | `183,418` |

- Agent tokens: `1,575,598`
- LLM judge tokens: `0` (no LLM judge)
- Total model tokens: `1,575,598`
- Count source: provider transcripts. Models are counted per assistant turn in each transcript. Tokens are omp `usage.totalTokens` summed per turn, Codex final `total_token_usage.total_tokens`, and Claude input + output + cache-read + cache-creation tokens summed per turn; cached reads dominate. The mission projection reported `usage: null`.

## Observable graph transitions

| Time | Subject or step | Transition | Evidence |
| --- | --- | --- | --- |
| `08:38:03.918` | `mission-run/cross-omp-hold-astra-20260927-a` | `absent -> created` | store index 3, mission-run.created |
| `08:38:03.941` | `exercise-message-wake` | `absent -> ready` | store index 9, step-run.state |
| `08:49:54.900` | `exercise-message-wake` | `working -> cancelled` | store index 134, step-run.state |
| `08:49:54.900` | `held-out-gates` | `absent -> cancelled` | store index 135, step-run.state |
| `08:49:54.929` | `cleanup-agents` | `absent -> ready` | store index 139, step-run.state |
| `08:49:55.949` | `cleanup-agents` | `working -> completed` | store index 161, step-run.state |
| `08:49:55.977` | `mission-run/cross-omp-hold-astra-20260927-a` | `running -> cancelled` | store index 164, mission-run.state |

## Small Talk

- Runtime work messages: `0`
- Direct agent messages: `12`
- Person or controller messages: `4`
- Required sequence: per phase: the controller sends 4 kickoffs; each pair exchanges one `FACT`, then one matching `AGREEMENT`; each participant sends one `CONSENSUS` to `person/eval-requester`. The idle phase starts only after all four seats report exactly `idle`
- Unexpected or duplicate messages: the idle phase never started: 4 kickoffs and 12 protocol messages, each sent by its real seat

## Judges

| Judge | Result | Duration | Evidence |
| --- | --- | ---: | --- |
| none reached | `n/a` | `n/a` | the mission stopped before any gate ran |
| terminal input audit | `pass` | `n/a` | `0` `terminal.input.requested` claims in the whole graph |

## Result details

- Products or commits: none beyond the graph records above
- Cleanup: complete
- Notable behavior: Startup: kickoffs sent at 08:38:07.543; receipts +5.2 s, facts +22.6 s, agreements +34.8 s, results +44.1 s.
- Notable behavior: Both omp `model_change` entries and every omp assistant turn record `openai-codex/gpt-6-astra`.
- Notable behavior: With the hold, no omp tool call was backgrounded by incoming mail (`wake.omp` 8 tool results; `wake.omp-2` 8 tool results). The omp channel handed the 6 messages for omp seats to omp a median 2.7 s after st3 staged them (at most 3.4 s); the omp seats read them a median 7.3 s after they were sent.
- Notable behavior: The startup phase completed. While the channel held Codex's `AGREEMENT EMBER+ORBIT` for `wake.omp`, the seat read it through the CLI, which records delivery and the read. When that batch returned, the channel's own delivery acknowledgement was refused as a `read -> delivered` transition, and st3's pi-family channel exited on the refusal. The seat finished its turn at 08:38:54, but with no channel st3 never again observed it idle. The controller's idle phase requires all four seats to report exactly idle, so it exited 1 after its deadline and the runner cancelled the run.
- Notable behavior: Earlier in the phase, Codex's `FACT EMBER` reached the seat 260 ms after a run ended; this version waited for omp's idle proof and started a new prompt with it, which ran normally. `c19aa71`'s commit message blames that prompt for the stall; `7b9d489` found and fixed the refused acknowledgement instead.
- omp failures:
  - `wake.omp`'s st3 channel exited on a refused late delivery acknowledgement (a runtime fault that the hold exposed)
- Follow-up: fixed in `7b9d489`; `cross-omp-astra-h4-20260927-a` and `-b` repeated this configuration on the fix
