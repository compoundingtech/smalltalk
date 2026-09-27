# Eval run report — 2026-09-27

- Eval: `cross-harness-message-wake` (fixed Codex gpt-6-sol and Claude opus seats, each paired with a Codex seat on `gpt-6-luna`)
- Runtime: `st3`
- Run ID: `mission-run/cross-codex-luna-env2-a`
- Candidate commit: st3 binary built from `agent/message-envelope` `f824999a` with `crates/st3/src/boot.rs` restored to `main`: the `<smalltalk-message>` envelope without the new boot-contract sentence (ablation)
- Eval KDL SHA-256: `c77ee4c2fa23233dd1a70730f4360ac791ec34b78afd8db95040f55ce2315737` (`variants/codex-luna.kdl`)
- Result: `fail`

## Timing

- Started: `2026-09-27T14:04:18.374Z`
- Ended: `2026-09-27T14:09:31.231Z`
- Duration: `312.857`

## Model usage

| Role | Harness | Model | Tokens |
| --- | --- | --- | ---: |
| `wake.claude` | `claude` | `claude-opus-5-5 × 18 turns` | `739,429` |
| `wake.codex` | `codex` | `gpt-6-sol × 1 turns` | `150,760` |
| `wake.codex-luna` | `codex` | `gpt-6-luna × 2 turns` | `145,046` |
| `wake.codex-luna-2` | `codex` | `gpt-6-luna × 1 turns` | `227,954` |

- Agent tokens: `1,263,189`
- LLM judge tokens: `0` (no LLM judge)
- Total model tokens: `1,263,189`
- Count source: provider transcripts. Models are counted per assistant turn in each transcript. Tokens are omp `usage.totalTokens` summed per turn, Codex final `total_token_usage.total_tokens`, and Claude input + output + cache-read + cache-creation tokens summed per turn; cached reads dominate. The mission projection reported `usage: null`.

## Observable graph transitions

| Time | Subject or step | Transition | Evidence |
| --- | --- | --- | --- |
| `14:04:18.374` | `mission-run/cross-codex-luna-env2-a` | `absent -> created` | store index 3, mission-run.created |
| `14:04:18.399` | `exercise-message-wake` | `absent -> ready` | store index 9, step-run.state |
| `14:09:30.482` | `exercise-message-wake` | `working -> cancelled` | store index 119, step-run.state |
| `14:09:30.482` | `held-out-gates` | `absent -> cancelled` | store index 120, step-run.state |
| `14:09:30.509` | `cleanup-agents` | `absent -> ready` | store index 124, step-run.state |
| `14:09:31.208` | `cleanup-agents` | `working -> completed` | store index 146, step-run.state |
| `14:09:31.231` | `mission-run/cross-codex-luna-env2-a` | `running -> cancelled` | store index 149, mission-run.state |

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
- Notable behavior: Startup: kickoffs sent at 14:04:21.962; all receipts +5.2 s; fact stage never completed.
- Notable behavior: Provider transcripts contain 12 `<smalltalk-message id=` and 22 `[PING from st3]` occurrences (the Claude seat's channel notices keep the PING form).
- Notable behavior: Attention requests: `wake.codex-luna-2`: "Resolve blocked startup consensus work ownership" (that seat sent its startup fact, startup agreement, startup result messages).
- Notable behavior: The controller exited 1 when the fact stage of the startup phase timed out after 300 s; the runner then cancelled the run instead of waiting for the 35-minute step timeout.
- Follow-up: none
