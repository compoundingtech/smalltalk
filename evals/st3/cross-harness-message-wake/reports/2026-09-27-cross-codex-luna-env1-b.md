# Eval run report — 2026-09-27

- Eval: `cross-harness-message-wake` (fixed Codex gpt-6-sol and Claude opus seats, each paired with a Codex seat on `gpt-6-luna`)
- Runtime: `st3`
- Run ID: `mission-run/cross-codex-luna-env1-b`
- Candidate commit: st3 binary built from `agent/message-envelope` `f824999a`: the `<smalltalk-message>` envelope for Codex, OpenCode, pi and omp and the new boot-contract sentence
- Eval KDL SHA-256: `c77ee4c2fa23233dd1a70730f4360ac791ec34b78afd8db95040f55ce2315737` (`variants/codex-luna.kdl`)
- Result: `fail`

## Timing

- Started: `2026-09-27T13:42:11.188Z`
- Ended: `2026-09-27T13:50:45.887Z`
- Duration: `514.699`

## Model usage

| Role | Harness | Model | Tokens |
| --- | --- | --- | ---: |
| `wake.claude` | `claude` | `claude-opus-5-5 × 23 turns` | `970,648` |
| `wake.codex` | `codex` | `gpt-6-sol × 3 turns` | `349,504` |
| `wake.codex-luna` | `codex` | `gpt-6-luna × 4 turns` | `324,754` |
| `wake.codex-luna-2` | `codex` | `gpt-6-luna × 2 turns` | `293,122` |

- Agent tokens: `1,938,028`
- LLM judge tokens: `0` (no LLM judge)
- Total model tokens: `1,938,028`
- Count source: provider transcripts. Models are counted per assistant turn in each transcript. Tokens are omp `usage.totalTokens` summed per turn, Codex final `total_token_usage.total_tokens`, and Claude input + output + cache-read + cache-creation tokens summed per turn; cached reads dominate. The mission projection reported `usage: null`.

## Observable graph transitions

| Time | Subject or step | Transition | Evidence |
| --- | --- | --- | --- |
| `13:42:11.188` | `mission-run/cross-codex-luna-env1-b` | `absent -> created` | store index 3, mission-run.created |
| `13:42:11.212` | `exercise-message-wake` | `absent -> ready` | store index 9, step-run.state |
| `13:50:44.951` | `exercise-message-wake` | `working -> cancelled` | store index 199, step-run.state |
| `13:50:44.951` | `held-out-gates` | `absent -> cancelled` | store index 200, step-run.state |
| `13:50:44.979` | `cleanup-agents` | `absent -> ready` | store index 204, step-run.state |
| `13:50:45.851` | `cleanup-agents` | `working -> completed` | store index 225, step-run.state |
| `13:50:45.887` | `mission-run/cross-codex-luna-env1-b` | `running -> cancelled` | store index 228, mission-run.state |

## Small Talk

- Runtime work messages: `0`
- Direct agent messages: `19`
- Person or controller messages: `8`
- Required sequence: per phase: the controller sends 4 kickoffs; each pair exchanges one `FACT`, then one matching `AGREEMENT`; each participant sends one `CONSENSUS` to `person/eval-requester`. The idle phase starts only after all four seats report exactly `idle`
- Unexpected or duplicate messages: none (8 kickoffs, 19 protocol messages)

## Judges

| Judge | Result | Duration | Evidence |
| --- | --- | ---: | --- |
| none reached | `n/a` | `n/a` | the mission stopped before any gate ran |
| terminal input audit | `pass` | `n/a` | `0` `terminal.input.requested` claims in the whole graph |

## Result details

- Products or commits: none beyond the graph records above
- Cleanup: complete
- Notable behavior: Startup: kickoffs sent at 13:42:14.612; all receipts +9.4 s; fact stage complete +118.7 s; agreement stage complete +122.9 s; result stage complete +170.7 s.
- Notable behavior: Idle: kickoffs sent at 13:45:36.145; all receipts +4.2 s; fact stage never completed.
- Notable behavior: Provider transcripts contain 34 `<smalltalk-message id=` and 24 `[PING from st3]` occurrences (the Claude seat's channel notices keep the PING form).
- Notable behavior: Attention requests: `wake.codex-luna-2`: "Close completed startup consensus work step" (that seat sent its startup fact, startup agreement, startup result messages); `wake.codex-luna-2`: "Make idle consensus phase claimable" (that seat sent its startup fact, startup agreement, startup result messages).
- Notable behavior: The controller exited 1 when the fact stage of the idle phase timed out after 300 s; the runner then cancelled the run instead of waiting for the 35-minute step timeout.
- Follow-up: none
