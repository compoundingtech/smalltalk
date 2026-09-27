# Eval run report — 2026-09-27

- Eval: `cross-harness-message-wake` (fixed Codex gpt-6-sol and Claude opus seats, each paired with a Codex seat on `gpt-6-luna`)
- Runtime: `st3`
- Run ID: `mission-run/cross-codex-luna-env2-b`
- Candidate commit: st3 binary built from `agent/message-envelope` `f824999a` with `crates/st3/src/boot.rs` restored to `main`: the `<smalltalk-message>` envelope without the new boot-contract sentence (ablation)
- Eval KDL SHA-256: `c77ee4c2fa23233dd1a70730f4360ac791ec34b78afd8db95040f55ce2315737` (`variants/codex-luna.kdl`)
- Result: `fail`

## Timing

- Started: `2026-09-27T14:09:55.969Z`
- Ended: `2026-09-27T14:15:08.840Z`
- Duration: `312.871`

## Model usage

| Role | Harness | Model | Tokens |
| --- | --- | --- | ---: |
| `wake.claude` | `claude` | `claude-opus-5-5 × 25 turns` | `1,062,349` |
| `wake.codex` | `codex` | `gpt-6-sol × 1 turns` | `115,020` |
| `wake.codex-luna` | `codex` | `gpt-6-luna × 1 turns` | `111,291` |
| `wake.codex-luna-2` | `codex` | `gpt-6-luna × 2 turns` | `166,845` |

- Agent tokens: `1,455,505`
- LLM judge tokens: `0` (no LLM judge)
- Total model tokens: `1,455,505`
- Count source: provider transcripts. Models are counted per assistant turn in each transcript. Tokens are omp `usage.totalTokens` summed per turn, Codex final `total_token_usage.total_tokens`, and Claude input + output + cache-read + cache-creation tokens summed per turn; cached reads dominate. The mission projection reported `usage: null`.

## Observable graph transitions

| Time | Subject or step | Transition | Evidence |
| --- | --- | --- | --- |
| `14:09:55.969` | `mission-run/cross-codex-luna-env2-b` | `absent -> created` | store index 3, mission-run.created |
| `14:09:55.992` | `exercise-message-wake` | `absent -> ready` | store index 9, step-run.state |
| `14:15:07.874` | `exercise-message-wake` | `working -> cancelled` | store index 119, step-run.state |
| `14:15:07.874` | `held-out-gates` | `absent -> cancelled` | store index 120, step-run.state |
| `14:15:07.905` | `cleanup-agents` | `absent -> ready` | store index 124, step-run.state |
| `14:15:08.814` | `cleanup-agents` | `working -> completed` | store index 146, step-run.state |
| `14:15:08.840` | `mission-run/cross-codex-luna-env2-b` | `running -> cancelled` | store index 149, mission-run.state |

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
- Notable behavior: Startup: kickoffs sent at 14:09:59.677; all receipts +5.3 s; fact stage never completed.
- Notable behavior: Provider transcripts contain 12 `<smalltalk-message id=` and 22 `[PING from st3]` occurrences (the Claude seat's channel notices keep the PING form).
- Notable behavior: Attention requests: `wake.codex-luna`: "Publish startup consensus as claimable st3 work" (that seat sent no protocol message).
- Notable behavior: The controller exited 1 when the fact stage of the startup phase timed out after 300 s; the runner then cancelled the run instead of waiting for the 35-minute step timeout.
- Follow-up: none
