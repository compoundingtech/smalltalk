# Eval run report — 2026-09-27

- Eval: `cross-harness-message-wake` (fixed Codex gpt-6-sol and Claude opus seats, each paired with a Codex seat on `gpt-6-luna`)
- Runtime: `st3`
- Run ID: `mission-run/cross-codex-luna-env1-c`
- Candidate commit: st3 binary built from `agent/message-envelope` `f824999a`: the `<smalltalk-message>` envelope for Codex, OpenCode, pi and omp and the new boot-contract sentence
- Eval KDL SHA-256: `c77ee4c2fa23233dd1a70730f4360ac791ec34b78afd8db95040f55ce2315737` (`variants/codex-luna.kdl`)
- Result: `fail`

## Timing

- Started: `2026-09-27T13:58:14.904Z`
- Ended: `2026-09-27T14:03:38.353Z`
- Duration: `323.449`

## Model usage

| Role | Harness | Model | Tokens |
| --- | --- | --- | ---: |
| `wake.claude` | `claude` | `claude-opus-5-5 × 22 turns` | `925,648` |
| `wake.codex` | `codex` | `gpt-6-sol × 1 turns` | `172,745` |
| `wake.codex-luna` | `codex` | `gpt-6-luna × 1 turns` | `237,733` |
| `wake.codex-luna-2` | `codex` | `gpt-6-luna × 1 turns` | `190,685` |

- Agent tokens: `1,526,811`
- LLM judge tokens: `0` (no LLM judge)
- Total model tokens: `1,526,811`
- Count source: provider transcripts. Models are counted per assistant turn in each transcript. Tokens are omp `usage.totalTokens` summed per turn, Codex final `total_token_usage.total_tokens`, and Claude input + output + cache-read + cache-creation tokens summed per turn; cached reads dominate. The mission projection reported `usage: null`.

## Observable graph transitions

| Time | Subject or step | Transition | Evidence |
| --- | --- | --- | --- |
| `13:58:14.904` | `mission-run/cross-codex-luna-env1-c` | `absent -> created` | store index 3, mission-run.created |
| `13:58:14.935` | `exercise-message-wake` | `absent -> ready` | store index 9, step-run.state |
| `14:03:37.495` | `exercise-message-wake` | `working -> cancelled` | store index 126, step-run.state |
| `14:03:37.495` | `held-out-gates` | `absent -> cancelled` | store index 127, step-run.state |
| `14:03:37.525` | `cleanup-agents` | `absent -> ready` | store index 131, step-run.state |
| `14:03:38.330` | `cleanup-agents` | `working -> completed` | store index 153, step-run.state |
| `14:03:38.353` | `mission-run/cross-codex-luna-env1-c` | `running -> cancelled` | store index 156, mission-run.state |

## Small Talk

- Runtime work messages: `0`
- Direct agent messages: `8`
- Person or controller messages: `4`
- Required sequence: per phase: the controller sends 4 kickoffs; each pair exchanges one `FACT`, then one matching `AGREEMENT`; each participant sends one `CONSENSUS` to `person/eval-requester`. The idle phase starts only after all four seats report exactly `idle`
- Unexpected or duplicate messages: none (4 kickoffs, 8 protocol messages)

## Judges

| Judge | Result | Duration | Evidence |
| --- | --- | ---: | --- |
| none reached | `n/a` | `n/a` | the mission stopped before any gate ran |
| terminal input audit | `pass` | `n/a` | `0` `terminal.input.requested` claims in the whole graph |

## Result details

- Products or commits: none beyond the graph records above
- Cleanup: complete
- Notable behavior: Startup: kickoffs sent at 13:58:23.627; all receipts +9.4 s; fact stage never completed.
- Notable behavior: Provider transcripts contain 16 `<smalltalk-message id=` and 20 `[PING from st3]` occurrences (the Claude seat's channel notices keep the PING form).
- Notable behavior: Attention requests: `wake.codex-luna-2`: "Work step unavailable despite actionable listing" (that seat sent its startup agreement messages); `wake.codex-luna`: "Resolve ownership for completed consensus step" (that seat sent its startup fact, startup agreement, startup result messages).
- Notable behavior: The controller exited 1 when the fact stage of the startup phase timed out after 300 s; the runner then cancelled the run instead of waiting for the 35-minute step timeout.
- Follow-up: none
