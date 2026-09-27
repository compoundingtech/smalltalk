# Eval run report — 2026-09-27

- Eval: `cross-harness-message-wake` (fixed Codex gpt-6-sol and Claude opus seats, each paired with an omp seat on `openai-codex/gpt-5.6-luna`)
- Runtime: `st3`
- Run ID: `mission-run/cross-omp-hold-luna-20260927-a`
- Candidate commit: st3 binary built from the tree committed as `9ba4c1b` (the omp mail hold, before the hook-root fix `7e7d8d8`)
- Eval KDL SHA-256: `0459220519955da16f8d62164c8a8c832734796e85d8696ce79ea910d99b1720` (`variants/omp-luna.kdl`)
- Result: `void`

## Timing

- Started: `2026-09-27T08:27:12.849Z`
- Ended: `2026-09-27T08:34:33.732Z`
- Duration: `440.883`

## Model usage

| Role | Harness | Model | Tokens |
| --- | --- | --- | ---: |
| `wake.claude` | `claude` | `claude-opus-5-5 × 9 turns` | `351,995` |
| `wake.codex` | `codex` | `gpt-6-sol × 1 turns` | `59,748` |

- Agent tokens: `411,743`
- LLM judge tokens: `0` (no LLM judge)
- Total model tokens: `411,743`
- Count source: provider transcripts. Models are counted per assistant turn in each transcript. Tokens are omp `usage.totalTokens` summed per turn, Codex final `total_token_usage.total_tokens`, and Claude input + output + cache-read + cache-creation tokens summed per turn; cached reads dominate. The mission projection reported `usage: null`.

## Observable graph transitions

| Time | Subject or step | Transition | Evidence |
| --- | --- | --- | --- |
| `08:27:12.849` | `mission-run/cross-omp-hold-luna-20260927-a` | `absent -> created` | store index 3, mission-run.created |
| `08:27:12.870` | `exercise-message-wake` | `absent -> ready` | store index 9, step-run.state |
| `08:34:33.065` | `exercise-message-wake` | `working -> cancelled` | store index 65, step-run.state |
| `08:34:33.065` | `held-out-gates` | `absent -> cancelled` | store index 66, step-run.state |
| `08:34:33.093` | `cleanup-agents` | `absent -> ready` | store index 70, step-run.state |
| `08:34:33.710` | `cleanup-agents` | `working -> completed` | store index 86, step-run.state |
| `08:34:33.732` | `mission-run/cross-omp-hold-luna-20260927-a` | `running -> cancelled` | store index 89, mission-run.state |

## Small Talk

- Runtime work messages: `0`
- Direct agent messages: `0`
- Required sequence: per phase: the controller sends 4 kickoffs; each pair exchanges one `FACT`, then one matching `AGREEMENT`; each participant sends one `CONSENSUS` to `person/eval-requester`. The idle phase starts only after all four seats report exactly `idle`
- Unexpected or duplicate messages: the controller sent 4 startup kickoffs; no omp seat existed to answer them

## Judges

| Judge | Result | Duration | Evidence |
| --- | --- | ---: | --- |
| none reached | `n/a` | `n/a` | the mission stopped before any gate ran |
| terminal input audit | `pass` | `n/a` | `0` `terminal.input.requested` claims in the whole graph |

## Result details

- Products or commits: none beyond the graph records above
- Cleanup: complete
- Notable behavior: Void run. Both omp seats ended with `launch-error` before omp started, so no protocol exchange with omp could happen. st3 exports `ST_HOOKS` to members as its binary's hook set directory, and the pi-family launcher read it as the hook root, looking for `<set>/sets/<set>/`. Earlier omp runs launched only because the unchanged hook set held a stray nested copy of itself. The new omp channel made a new set without one. Fixed in `7e7d8d8`.
- Notable behavior: The runner cancelled the run about seven minutes after start. `cross-omp-hold-luna-20260927-e` to `-j` repeated this configuration with the fix.
- omp failures:
  - launch-error for `wake.omp` and `wake.omp-2`: a runtime fault in st3's hook-root handling, not an omp or model fault
- Follow-up: none; fixed in `7e7d8d8`
