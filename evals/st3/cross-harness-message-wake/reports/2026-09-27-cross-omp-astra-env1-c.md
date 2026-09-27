# Eval run report — 2026-09-27

- Eval: `cross-harness-message-wake` (fixed Codex gpt-6-sol and Claude opus seats, each paired with an omp seat on `openai-codex/gpt-6-astra`)
- Runtime: `st3`
- Run ID: `mission-run/cross-omp-astra-env1-c`
- Candidate commit: st3 binary built from `agent/message-envelope` `f824999a`: the `<smalltalk-message>` envelope for Codex, OpenCode, pi and omp and the new boot-contract sentence
- Eval KDL SHA-256: `0935dbe774a13dc1bf99b547c0dfee2df4b6a2124f3cac86ed7297805b54a3a7` (`variants/omp-astra.kdl`)
- Result: `fail`

## Timing

- Started: `2026-09-27T13:58:10.856Z`
- Ended: `2026-09-27T14:03:24.451Z`
- Duration: `313.595`

## Model usage

| Role | Harness | Model | Tokens |
| --- | --- | --- | ---: |
| `wake.claude` | `claude` | `claude-opus-5-5 × 15 turns` | `610,965` |
| `wake.codex` | `codex` | `gpt-6-sol × 1 turns` | `112,973` |
| `wake.omp` | `omp` | `openai-codex/gpt-6-astra × 7 turns` | `139,584` |
| `wake.omp-2` | `omp` | `openai-codex/gpt-6-astra × 8 turns` | `164,730` |

- Agent tokens: `1,028,252`
- LLM judge tokens: `0` (no LLM judge)
- Total model tokens: `1,028,252`
- Count source: provider transcripts. Models are counted per assistant turn in each transcript. Tokens are omp `usage.totalTokens` summed per turn, Codex final `total_token_usage.total_tokens`, and Claude input + output + cache-read + cache-creation tokens summed per turn; cached reads dominate. The mission projection reported `usage: null`.

## Observable graph transitions

| Time | Subject or step | Transition | Evidence |
| --- | --- | --- | --- |
| `13:58:10.856` | `mission-run/cross-omp-astra-env1-c` | `absent -> created` | store index 3, mission-run.created |
| `13:58:10.883` | `exercise-message-wake` | `absent -> ready` | store index 9, step-run.state |
| `14:03:23.266` | `exercise-message-wake` | `working -> cancelled` | store index 90, step-run.state |
| `14:03:23.266` | `held-out-gates` | `absent -> cancelled` | store index 91, step-run.state |
| `14:03:23.294` | `cleanup-agents` | `absent -> ready` | store index 95, step-run.state |
| `14:03:24.418` | `cleanup-agents` | `working -> completed` | store index 117, step-run.state |
| `14:03:24.451` | `mission-run/cross-omp-astra-env1-c` | `running -> cancelled` | store index 120, mission-run.state |

## Small Talk

- Runtime work messages: `0`
- Direct agent messages: `2`
- Person or controller messages: `4`
- Required sequence: per phase: the controller sends 4 kickoffs; each pair exchanges one `FACT`, then one matching `AGREEMENT`; each participant sends one `CONSENSUS` to `person/eval-requester`. The idle phase starts only after all four seats report exactly `idle`
- Unexpected or duplicate messages: none (4 kickoffs, 2 protocol messages)

## Judges

| Judge | Result | Duration | Evidence |
| --- | --- | ---: | --- |
| none reached | `n/a` | `n/a` | the mission stopped before any gate ran |
| terminal input audit | `pass` | `n/a` | `0` `terminal.input.requested` claims in the whole graph |

## Result details

- Products or commits: none beyond the graph records above
- Cleanup: complete
- Notable behavior: Startup: kickoffs sent at 13:58:15.112; all receipts +7.2 s; fact stage never completed.
- Notable behavior: Provider transcripts contain 6 `<smalltalk-message id=` and 14 `[PING from st3]` occurrences (the Claude seat's channel notices keep the PING form).
- Notable behavior: Attention requests: `wake.omp-2`: "Consensus startup has no claimable agent work" (that seat sent no protocol message); `wake.omp`: "Consensus participant lacks claimable graph work" (that seat sent no protocol message).
- Notable behavior: The controller exited 1 when the fact stage of the startup phase timed out after 300 s; the runner then cancelled the run instead of waiting for the 35-minute step timeout.
- omp failures:
  - `wake.omp` requested attention ("Consensus participant lacks claimable graph work") and did not send its startup `fact` message
  - `wake.omp-2` requested attention ("Consensus startup has no claimable agent work") and did not send its startup `fact` message
- Follow-up: none
