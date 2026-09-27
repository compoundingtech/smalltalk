# Eval run report — 2026-09-27

- Eval: `cross-harness-message-wake` (fixed Codex gpt-6-sol and Claude opus seats, each paired with a Codex seat on `gpt-6-luna`)
- Runtime: `st3`
- Run ID: `mission-run/cross-codex-luna-env0-c`
- Candidate commit: st3 binary built from `main` `513fc98b`, before the envelope (baseline)
- Eval KDL SHA-256: `c77ee4c2fa23233dd1a70730f4360ac791ec34b78afd8db95040f55ce2315737` (`variants/codex-luna.kdl`)
- Result: `fail`

## Timing

- Started: `2026-09-27T13:51:10.790Z`
- Ended: `2026-09-27T13:57:49.933Z`
- Duration: `399.143`

## Model usage

| Role | Harness | Model | Tokens |
| --- | --- | --- | ---: |
| `wake.claude` | `claude` | `claude-opus-5-5 × 31 turns` | `1,340,857` |
| `wake.codex` | `codex` | `gpt-6-sol × 2 turns` | `306,802` |
| `wake.codex-luna` | `codex` | `gpt-6-luna × 3 turns` | `313,331` |
| `wake.codex-luna-2` | `codex` | `gpt-6-luna × 2 turns` | `430,194` |

- Agent tokens: `2,391,184`
- LLM judge tokens: `0` (no LLM judge)
- Total model tokens: `2,391,184`
- Count source: provider transcripts. Models are counted per assistant turn in each transcript. Tokens are omp `usage.totalTokens` summed per turn, Codex final `total_token_usage.total_tokens`, and Claude input + output + cache-read + cache-creation tokens summed per turn; cached reads dominate. The mission projection reported `usage: null`.

## Observable graph transitions

| Time | Subject or step | Transition | Evidence |
| --- | --- | --- | --- |
| `13:51:10.790` | `mission-run/cross-codex-luna-env0-c` | `absent -> created` | store index 3, mission-run.created |
| `13:51:10.814` | `exercise-message-wake` | `absent -> ready` | store index 9, step-run.state |
| `13:57:48.530` | `exercise-message-wake` | `working -> cancelled` | store index 198, step-run.state |
| `13:57:48.530` | `held-out-gates` | `absent -> cancelled` | store index 199, step-run.state |
| `13:57:48.559` | `cleanup-agents` | `absent -> ready` | store index 203, step-run.state |
| `13:57:49.910` | `cleanup-agents` | `working -> completed` | store index 225, step-run.state |
| `13:57:49.933` | `mission-run/cross-codex-luna-env0-c` | `running -> cancelled` | store index 228, mission-run.state |

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
- Notable behavior: Startup: kickoffs sent at 13:51:14.338; all receipts +6.2 s; fact stage complete +35.8 s; agreement stage complete +43.0 s; result stage complete +57.5 s.
- Notable behavior: Idle: kickoffs sent at 13:52:45.430; all receipts +3.4 s; fact stage never completed.
- Notable behavior: Provider transcripts contain 0 `<smalltalk-message id=` and 62 `[PING from st3]` occurrences (baseline envelope).
- Notable behavior: Attention requests: `wake.codex-luna`: "Reassign completed startup consensus work" (that seat sent its startup fact, startup agreement, startup result messages); `wake.codex-luna-2`: "Resolve agentless work ownership for consensus step" (that seat sent its startup fact, startup agreement, startup result, idle fact, idle agreement, idle result messages); `wake.codex-luna`: "Publish idle consensus phase as claimable work" (that seat sent its startup fact, startup agreement, startup result messages).
- Notable behavior: The controller exited 1 when the fact stage of the idle phase timed out after 300 s; the runner then cancelled the run instead of waiting for the 35-minute step timeout.
- Follow-up: none
