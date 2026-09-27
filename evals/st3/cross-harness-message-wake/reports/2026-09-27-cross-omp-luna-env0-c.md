# Eval run report — 2026-09-27

- Eval: `cross-harness-message-wake` (fixed Codex gpt-6-sol and Claude opus seats, each paired with an omp seat on `openai-codex/gpt-5.6-luna`)
- Runtime: `st3`
- Run ID: `mission-run/cross-omp-luna-env0-c`
- Candidate commit: st3 binary built from `main` `513fc98b`, before the envelope (baseline)
- Eval KDL SHA-256: `0459220519955da16f8d62164c8a8c832734796e85d8696ce79ea910d99b1720` (`variants/omp-luna.kdl`)
- Result: `fail`

## Timing

- Started: `2026-09-27T13:51:02.741Z`
- Ended: `2026-09-27T13:56:17.019Z`
- Duration: `314.278`

## Model usage

| Role | Harness | Model | Tokens |
| --- | --- | --- | ---: |
| `wake.claude` | `claude` | `claude-opus-5-5 × 18 turns` | `731,371` |
| `wake.codex` | `codex` | `gpt-6-sol × 1 turns` | `185,337` |
| `wake.omp` | `omp` | `openai-codex/gpt-5.6-luna × 7 turns` | `142,188` |
| `wake.omp-2` | `omp` | `openai-codex/gpt-5.6-luna × 13 turns` | `268,946` |

- Agent tokens: `1,327,842`
- LLM judge tokens: `0` (no LLM judge)
- Total model tokens: `1,327,842`
- Count source: provider transcripts. Models are counted per assistant turn in each transcript. Tokens are omp `usage.totalTokens` summed per turn, Codex final `total_token_usage.total_tokens`, and Claude input + output + cache-read + cache-creation tokens summed per turn; cached reads dominate. The mission projection reported `usage: null`.

## Observable graph transitions

| Time | Subject or step | Transition | Evidence |
| --- | --- | --- | --- |
| `13:51:02.741` | `mission-run/cross-omp-luna-env0-c` | `absent -> created` | store index 3, mission-run.created |
| `13:51:02.762` | `exercise-message-wake` | `absent -> ready` | store index 9, step-run.state |
| `13:56:14.966` | `exercise-message-wake` | `working -> cancelled` | store index 106, step-run.state |
| `13:56:14.967` | `held-out-gates` | `absent -> cancelled` | store index 107, step-run.state |
| `13:56:14.993` | `cleanup-agents` | `absent -> ready` | store index 111, step-run.state |
| `13:56:16.996` | `cleanup-agents` | `working -> completed` | store index 133, step-run.state |
| `13:56:17.019` | `mission-run/cross-omp-luna-env0-c` | `running -> cancelled` | store index 136, mission-run.state |

## Small Talk

- Runtime work messages: `0`
- Direct agent messages: `5`
- Person or controller messages: `4`
- Required sequence: per phase: the controller sends 4 kickoffs; each pair exchanges one `FACT`, then one matching `AGREEMENT`; each participant sends one `CONSENSUS` to `person/eval-requester`. The idle phase starts only after all four seats report exactly `idle`
- Unexpected or duplicate messages: none (4 kickoffs, 5 protocol messages)

## Judges

| Judge | Result | Duration | Evidence |
| --- | --- | ---: | --- |
| none reached | `n/a` | `n/a` | the mission stopped before any gate ran |
| terminal input audit | `pass` | `n/a` | `0` `terminal.input.requested` claims in the whole graph |

## Result details

- Products or commits: none beyond the graph records above
- Cleanup: complete
- Notable behavior: Startup: kickoffs sent at 13:51:06.665; all receipts +8.2 s; fact stage never completed.
- Notable behavior: Provider transcripts contain 0 `<smalltalk-message id=` and 26 `[PING from st3]` occurrences (baseline envelope).
- Notable behavior: Attention requests: `wake.codex`: "Consensus participant needs claimable work" (that seat sent no protocol message).
- Notable behavior: The controller exited 1 when the fact stage of the startup phase timed out after 300 s; the runner then cancelled the run instead of waiting for the 35-minute step timeout.
- omp failures:
  - `wake.omp-2` did not send its startup `agreement` message
- Follow-up: none
