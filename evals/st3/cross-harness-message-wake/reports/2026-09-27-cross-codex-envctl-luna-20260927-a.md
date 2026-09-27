# Eval run report — 2026-09-27

- Eval: `cross-harness-message-wake` (fixed Codex gpt-6-sol and Claude opus seats, each paired with a Codex seat on `gpt-6-luna`)
- Runtime: `st3`
- Run ID: `mission-run/cross-codex-envctl-luna-20260927-a`
- Candidate commit: st3 binary built from `main` `513fc98b`, before the envelope (baseline)
- Eval KDL SHA-256: `c77ee4c2fa23233dd1a70730f4360ac791ec34b78afd8db95040f55ce2315737` (`variants/codex-luna.kdl`)
- Result: `stopped`

## Timing

- Started: `2026-09-27T12:46:09.955Z`
- Ended: `2026-09-27T13:03:10.245Z`
- Duration: `1020.290`

## Model usage

| Role | Harness | Model | Tokens |
| --- | --- | --- | ---: |

- Agent tokens: `0`
- LLM judge tokens: `0` (no LLM judge)
- Total model tokens: `0`
- Count source: provider transcripts. Models are counted per assistant turn in each transcript. Tokens are omp `usage.totalTokens` summed per turn, Codex final `total_token_usage.total_tokens`, and Claude input + output + cache-read + cache-creation tokens summed per turn; cached reads dominate. The mission projection reported `usage: null`.

## Observable graph transitions

| Time | Subject or step | Transition | Evidence |
| --- | --- | --- | --- |
| `12:46:09.955` | `mission-run/cross-codex-envctl-luna-20260927-a` | `absent -> created` | store index 3, mission-run.created |
| `12:46:09.978` | `exercise-message-wake` | `absent -> ready` | store index 9, step-run.state |
| `13:03:09.644` | `exercise-message-wake` | `working -> cancelled` | store index 4094, step-run.state |
| `13:03:09.645` | `held-out-gates` | `absent -> cancelled` | store index 4095, step-run.state |
| `13:03:09.827` | `cleanup-agents` | `absent -> ready` | store index 4100, step-run.state |
| `13:03:10.130` | `cleanup-agents` | `working -> completed` | store index 4107, step-run.state |
| `13:03:10.245` | `mission-run/cross-codex-envctl-luna-20260927-a` | `running -> cancelled` | store index 4114, mission-run.state |

## Small Talk

- Runtime work messages: `0`
- Direct agent messages: `0`
- Required sequence: per phase: the controller sends 4 kickoffs; each pair exchanges one `FACT`, then one matching `AGREEMENT`; each participant sends one `CONSENSUS` to `person/eval-requester`. The idle phase starts only after all four seats report exactly `idle`
- Unexpected or duplicate messages: none (0 kickoffs, 0 protocol messages)

## Judges

| Judge | Result | Duration | Evidence |
| --- | --- | ---: | --- |
| none reached | `n/a` | `n/a` | the mission stopped before any gate ran |
| terminal input audit | `pass` | `n/a` | `0` `terminal.input.requested` claims in the whole graph |

## Result details

- Products or commits: none beyond the graph records above
- Cleanup: complete
- Notable behavior: Provider transcripts contain 0 `<smalltalk-message id=` and 0 `[PING from st3]` occurrences (baseline envelope).
- Notable behavior: Attention requests: `reconciler`: "A Codex agent stopped after repeated failures" (runtime); `reconciler`: "A Codex agent stopped after repeated failures" (runtime); `reconciler`: "A Codex agent stopped after repeated failures" (runtime).
- Notable behavior: Eval fault, not a harness or model result: all three Codex seats (`wake.codex`, `wake.codex-luna`, `wake.codex-luna-2`) failed to launch three times because the run ID made their PTY socket paths 107 to 110 bytes, over the 104-byte limit. st3 raised "A Codex agent stopped after repeated failures" for each. The runner cancelled the run at 13:03 UTC and it was repeated under a shorter run ID. It is excluded from the before/after counts.
- Follow-up: Runner: keep run IDs short enough that the PTY socket path stays under 104 bytes
