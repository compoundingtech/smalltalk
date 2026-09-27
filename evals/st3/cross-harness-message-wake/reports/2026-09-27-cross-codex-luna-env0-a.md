# Eval run report — 2026-09-27

- Eval: `cross-harness-message-wake` (fixed Codex gpt-6-sol and Claude opus seats, each paired with a Codex seat on `gpt-6-luna`)
- Runtime: `st3`
- Run ID: `mission-run/cross-codex-luna-env0-a`
- Candidate commit: st3 binary built from `main` `513fc98b`, before the envelope (baseline)
- Eval KDL SHA-256: `c77ee4c2fa23233dd1a70730f4360ac791ec34b78afd8db95040f55ce2315737` (`variants/codex-luna.kdl`)
- Result: `pass`

## Timing

- Started: `2026-09-27T13:03:36.067Z`
- Ended: `2026-09-27T13:06:01.763Z`
- Duration: `145.696`

## Model usage

| Role | Harness | Model | Tokens |
| --- | --- | --- | ---: |
| `wake.claude` | `claude` | `claude-opus-5-5 × 26 turns` | `1,121,977` |
| `wake.codex` | `codex` | `gpt-6-sol × 2 turns` | `431,012` |
| `wake.codex-luna` | `codex` | `gpt-6-luna × 4 turns` | `433,583` |
| `wake.codex-luna-2` | `codex` | `gpt-6-luna × 3 turns` | `241,389` |

- Agent tokens: `2,227,961`
- LLM judge tokens: `0` (no LLM judge)
- Total model tokens: `2,227,961`
- Count source: provider transcripts. Models are counted per assistant turn in each transcript. Tokens are omp `usage.totalTokens` summed per turn, Codex final `total_token_usage.total_tokens`, and Claude input + output + cache-read + cache-creation tokens summed per turn; cached reads dominate. The mission projection reported `usage: null`.

## Observable graph transitions

| Time | Subject or step | Transition | Evidence |
| --- | --- | --- | --- |
| `13:03:36.067` | `mission-run/cross-codex-luna-env0-a` | `absent -> created` | store index 3, mission-run.created |
| `13:03:36.089` | `exercise-message-wake` | `absent -> ready` | store index 9, step-run.state |
| `13:05:59.866` | `exercise-message-wake` | `working -> completed` | store index 216, step-run.state |
| `13:05:59.896` | `held-out-gates` | `absent -> ready` | store index 217, step-run.state |
| `13:06:00.994` | `held-out-gates` | `working -> completed` | store index 235, step-run.state |
| `13:06:01.050` | `cleanup-agents` | `absent -> ready` | store index 238, step-run.state |
| `13:06:01.740` | `cleanup-agents` | `working -> completed` | store index 260, step-run.state |
| `13:06:01.763` | `mission-run/cross-codex-luna-env0-a` | `running -> completed` | store index 263, mission-run.state |

## Small Talk

- Runtime work messages: `0`
- Direct agent messages: `24`
- Person or controller messages: `8`
- Required sequence: per phase: the controller sends 4 kickoffs; each pair exchanges one `FACT`, then one matching `AGREEMENT`; each participant sends one `CONSENSUS` to `person/eval-requester`. The idle phase starts only after all four seats report exactly `idle`
- Unexpected or duplicate messages: none (8 kickoffs, 24 protocol messages)

## Judges

| Judge | Result | Duration | Evidence |
| --- | --- | ---: | --- |
| the controller observed both final reports | `pass` | `n/a` | stage `exercise-message-wake` |
| the canonical conversation proves wake, exchange, and consensus | `pass` | `0.821` | stage `held-out-gates` |
| no terminal input path participated | `pass` | `0.247` | stage `held-out-gates` |
| terminal input audit | `pass` | `n/a` | `0` `terminal.input.requested` claims in the whole graph |

## Result details

- Products or commits: none beyond the graph records above
- Cleanup: complete
- Notable behavior: Startup: kickoffs sent at 13:03:39.421; all receipts +6.2 s; fact stage complete +41.8 s; agreement stage complete +53.1 s; result stage complete +66.4 s.
- Notable behavior: Idle: kickoffs sent at 13:05:29.024; all receipts +3.2 s; fact stage complete +11.3 s; agreement stage complete +22.6 s; result stage complete +30.8 s.
- Notable behavior: Provider transcripts contain 0 `<smalltalk-message id=` and 66 `[PING from st3]` occurrences (baseline envelope).
- Notable behavior: Attention requests: `wake.codex-luna`: "Assign or finish agentless startup work" (that seat sent its startup fact, startup agreement, startup result, idle fact, idle agreement, idle result messages).
- Follow-up: none
