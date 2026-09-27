# Eval run report — 2026-09-27

- Eval: `cross-harness-message-wake` (fixed Codex gpt-6-sol and Claude opus seats, each paired with a Codex seat on `gpt-6-luna`)
- Runtime: `st3`
- Run ID: `mission-run/cross-codex-luna-env1-a`
- Candidate commit: st3 binary built from `agent/message-envelope` `f824999a`: the `<smalltalk-message>` envelope for Codex, OpenCode, pi and omp and the new boot-contract sentence
- Eval KDL SHA-256: `c77ee4c2fa23233dd1a70730f4360ac791ec34b78afd8db95040f55ce2315737` (`variants/codex-luna.kdl`)
- Result: `fail`

## Timing

- Started: `2026-09-27T13:06:26.931Z`
- Ended: `2026-09-27T13:36:09.846Z`
- Duration: `1782.915`

## Model usage

| Role | Harness | Model | Tokens |
| --- | --- | --- | ---: |
| `wake.claude` | `claude` | `claude-opus-5-5 × 14 turns` | `566,298` |
| `wake.codex` | `codex` | `gpt-6-sol × 1 turns` | `139,795` |
| `wake.codex-luna` | `codex` | `gpt-6-luna × 1 turns` | `129,026` |
| `wake.codex-luna-2` | `codex` | `gpt-6-luna × 1 turns` | `94,278` |

- Agent tokens: `929,397`
- LLM judge tokens: `0` (no LLM judge)
- Total model tokens: `929,397`
- Count source: provider transcripts. Models are counted per assistant turn in each transcript. Tokens are omp `usage.totalTokens` summed per turn, Codex final `total_token_usage.total_tokens`, and Claude input + output + cache-read + cache-creation tokens summed per turn; cached reads dominate. The mission projection reported `usage: null`.

## Observable graph transitions

| Time | Subject or step | Transition | Evidence |
| --- | --- | --- | --- |
| `13:06:26.931` | `mission-run/cross-codex-luna-env1-a` | `absent -> created` | store index 3, mission-run.created |
| `13:06:26.956` | `exercise-message-wake` | `absent -> ready` | store index 9, step-run.state |
| `13:36:08.851` | `exercise-message-wake` | `working -> cancelled` | store index 112, step-run.state |
| `13:36:08.851` | `held-out-gates` | `absent -> cancelled` | store index 113, step-run.state |
| `13:36:08.884` | `cleanup-agents` | `absent -> ready` | store index 117, step-run.state |
| `13:36:09.824` | `cleanup-agents` | `working -> completed` | store index 139, step-run.state |
| `13:36:09.846` | `mission-run/cross-codex-luna-env1-a` | `running -> cancelled` | store index 142, mission-run.state |

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
- Notable behavior: Startup: kickoffs sent at 13:06:35.446; all receipts +8.5 s; fact stage never completed.
- Notable behavior: Provider transcripts contain 10 `<smalltalk-message id=` and 18 `[PING from st3]` occurrences (the Claude seat's channel notices keep the PING form).
- Notable behavior: Attention requests: `wake.codex-luna-2`: "Publish startup consensus as claimable ST3 work" (that seat sent no protocol message); `wake.codex`: "Consensus startup has no claimable agent step" (that seat sent no protocol message).
- Notable behavior: The controller exited 1 when the startup fact stage timed out after 300 s; the run was cancelled at 13:36 UTC instead of waiting for the 35-minute step timeout. `wake.codex-luna` sent its fact, and Claude sent `FACT QUARTZ`. `wake.codex-luna-2` and the fixed `wake.codex` seat each read their kickoff and their peer's fact, found no claimable step, and asked for one ("Per .st3/boot.md, authorized work must be exposed as active graph work and claimed before starting"). Neither sent a fact.
- Follow-up: Expose each participant's protocol as a claimable step, or accept message-only coordination in the boot contract
