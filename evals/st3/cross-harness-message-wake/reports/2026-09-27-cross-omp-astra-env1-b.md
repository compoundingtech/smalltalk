# Eval run report — 2026-09-27

- Eval: `cross-harness-message-wake` (fixed Codex gpt-6-sol and Claude opus seats, each paired with an omp seat on `openai-codex/gpt-6-astra`)
- Runtime: `st3`
- Run ID: `mission-run/cross-omp-astra-env1-b`
- Candidate commit: st3 binary built from `agent/message-envelope` `f824999a`: the `<smalltalk-message>` envelope for Codex, OpenCode, pi and omp and the new boot-contract sentence
- Eval KDL SHA-256: `0935dbe774a13dc1bf99b547c0dfee2df4b6a2124f3cac86ed7297805b54a3a7` (`variants/omp-astra.kdl`)
- Result: `fail`

## Timing

- Started: `2026-09-27T13:42:07.173Z`
- Ended: `2026-09-27T13:47:20.558Z`
- Duration: `313.385`

## Model usage

| Role | Harness | Model | Tokens |
| --- | --- | --- | ---: |
| `wake.claude` | `claude` | `claude-opus-5-5 × 19 turns` | `806,367` |
| `wake.codex` | `codex` | `gpt-6-sol × 1 turns` | `186,821` |
| `wake.omp` | `omp` | `openai-codex/gpt-6-astra × 8 turns` | `161,178` |
| `wake.omp-2` | `omp` | `openai-codex/gpt-6-astra × 9 turns` | `187,210` |

- Agent tokens: `1,341,576`
- LLM judge tokens: `0` (no LLM judge)
- Total model tokens: `1,341,576`
- Count source: provider transcripts. Models are counted per assistant turn in each transcript. Tokens are omp `usage.totalTokens` summed per turn, Codex final `total_token_usage.total_tokens`, and Claude input + output + cache-read + cache-creation tokens summed per turn; cached reads dominate. The mission projection reported `usage: null`.

## Observable graph transitions

| Time | Subject or step | Transition | Evidence |
| --- | --- | --- | --- |
| `13:42:07.173` | `mission-run/cross-omp-astra-env1-b` | `absent -> created` | store index 3, mission-run.created |
| `13:42:07.194` | `exercise-message-wake` | `absent -> ready` | store index 9, step-run.state |
| `13:47:19.411` | `exercise-message-wake` | `working -> cancelled` | store index 115, step-run.state |
| `13:47:19.411` | `held-out-gates` | `absent -> cancelled` | store index 116, step-run.state |
| `13:47:19.436` | `cleanup-agents` | `absent -> ready` | store index 120, step-run.state |
| `13:47:20.535` | `cleanup-agents` | `working -> completed` | store index 142, step-run.state |
| `13:47:20.558` | `mission-run/cross-omp-astra-env1-b` | `running -> cancelled` | store index 145, mission-run.state |

## Small Talk

- Runtime work messages: `0`
- Direct agent messages: `7`
- Person or controller messages: `4`
- Required sequence: per phase: the controller sends 4 kickoffs; each pair exchanges one `FACT`, then one matching `AGREEMENT`; each participant sends one `CONSENSUS` to `person/eval-requester`. The idle phase starts only after all four seats report exactly `idle`
- Unexpected or duplicate messages: none (4 kickoffs, 7 protocol messages)

## Judges

| Judge | Result | Duration | Evidence |
| --- | --- | ---: | --- |
| none reached | `n/a` | `n/a` | the mission stopped before any gate ran |
| terminal input audit | `pass` | `n/a` | `0` `terminal.input.requested` claims in the whole graph |

## Result details

- Products or commits: none beyond the graph records above
- Cleanup: complete
- Notable behavior: Startup: kickoffs sent at 13:42:10.679; all receipts +6.3 s; fact stage never completed.
- Notable behavior: Provider transcripts contain 7 `<smalltalk-message id=` and 20 `[PING from st3]` occurrences (the Claude seat's channel notices keep the PING form).
- Notable behavior: Attention requests: `wake.omp`: "Consensus startup requires graph work" (that seat sent no protocol message).
- Notable behavior: The controller exited 1 when the fact stage of the startup phase timed out after 300 s; the runner then cancelled the run instead of waiting for the 35-minute step timeout.
- omp failures:
  - `wake.omp` requested attention ("Consensus startup requires graph work") and did not send its startup `fact` message
- Follow-up: none
