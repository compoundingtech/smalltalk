# Eval run report — 2026-09-27

- Eval: `cross-harness-message-wake` (fixed Codex gpt-6-sol and Claude opus seats, each paired with a Codex seat on `gpt-6-luna`)
- Runtime: `st3`
- Run ID: `mission-run/cross-codex-luna-env0-b`
- Candidate commit: st3 binary built from `main` `513fc98b`, before the envelope (baseline)
- Eval KDL SHA-256: `c77ee4c2fa23233dd1a70730f4360ac791ec34b78afd8db95040f55ce2315737` (`variants/codex-luna.kdl`)
- Result: `fail`

## Timing

- Started: `2026-09-27T13:36:34.313Z`
- Ended: `2026-09-27T13:41:46.954Z`
- Duration: `312.641`

## Model usage

| Role | Harness | Model | Tokens |
| --- | --- | --- | ---: |
| `wake.claude` | `claude` | `claude-opus-5-5 × 21 turns` | `880,190` |
| `wake.codex` | `codex` | `gpt-6-sol × 1 turns` | `112,535` |
| `wake.codex-luna` | `codex` | `gpt-6-luna × 1 turns` | `133,248` |
| `wake.codex-luna-2` | `codex` | `gpt-6-luna × 1 turns` | `227,194` |

- Agent tokens: `1,353,167`
- LLM judge tokens: `0` (no LLM judge)
- Total model tokens: `1,353,167`
- Count source: provider transcripts. Models are counted per assistant turn in each transcript. Tokens are omp `usage.totalTokens` summed per turn, Codex final `total_token_usage.total_tokens`, and Claude input + output + cache-read + cache-creation tokens summed per turn; cached reads dominate. The mission projection reported `usage: null`.

## Observable graph transitions

| Time | Subject or step | Transition | Evidence |
| --- | --- | --- | --- |
| `13:36:34.313` | `mission-run/cross-codex-luna-env0-b` | `absent -> created` | store index 3, mission-run.created |
| `13:36:34.336` | `exercise-message-wake` | `absent -> ready` | store index 9, step-run.state |
| `13:41:46.054` | `exercise-message-wake` | `working -> cancelled` | store index 118, step-run.state |
| `13:41:46.054` | `held-out-gates` | `absent -> cancelled` | store index 119, step-run.state |
| `13:41:46.081` | `cleanup-agents` | `absent -> ready` | store index 123, step-run.state |
| `13:41:46.929` | `cleanup-agents` | `working -> completed` | store index 145, step-run.state |
| `13:41:46.954` | `mission-run/cross-codex-luna-env0-b` | `running -> cancelled` | store index 148, mission-run.state |

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
- Notable behavior: Startup: kickoffs sent at 13:36:37.719; all receipts +6.2 s; fact stage never completed.
- Notable behavior: Provider transcripts contain 0 `<smalltalk-message id=` and 34 `[PING from st3]` occurrences (baseline envelope).
- Notable behavior: Attention requests: `wake.codex-luna`: "Unclaimable consensus startup work" (that seat sent no protocol message); `wake.codex-luna-2`: "Finalize startup consensus work item" (that seat sent its startup fact, startup agreement, startup result messages).
- Notable behavior: The controller exited 1 when the fact stage of the startup phase timed out after 300 s; the runner then cancelled the run instead of waiting for the 35-minute step timeout.
- Follow-up: none
