# Eval run report — 2026-09-26

- Eval: `restart-continuity` (committed eval: rc.dev and rc.sup on Claude `claude-sonnet-5`)
- Runtime: `st3`
- Run ID: `mission-run/restart-sonnet-final-20260926-a`
- Candidate commit: st3 binary built from `f6c76b6`, which also listed boot-contract work with `--as "$ST_AGENT"`; `72c04ce` reverted that listing; eval files with the fixture fix in `8dd9cbb`
- Eval KDL SHA-256: `0bb121739de7e64385bf37fc6a0bf65dfbbd2c538ce061a668f5f9f743922cb4` (`eval.kdl`)
- Result: `fail`

## Timing

- Started: `2026-09-26T23:12:22.298Z`
- Ended: `2026-09-26T23:22:23.576Z`
- Duration: `601.278`

## Model usage

| Role | Harness | Model | Tokens |
| --- | --- | --- | ---: |
| `rc.dev` | `claude` | `claude-sonnet-5 × 2 turns` | `74,253` |

- Agent tokens: `74,253`
- LLM judge tokens: `0` (no LLM judge)
- Total model tokens: `74,253`
- Count source: provider transcripts. Models are counted per assistant turn in each transcript. Tokens are omp `usage.totalTokens` summed per turn, Codex final `total_token_usage.total_tokens`, and Claude input + output + cache-read + cache-creation tokens summed per turn; cached reads dominate. The mission projection reported `usage: null`.

## Observable graph transitions

| Time | Subject or step | Transition | Evidence |
| --- | --- | --- | --- |
| `23:12:22.298` | `mission-run/restart-sonnet-final-20260926-a` | `absent -> created` | store index 3, mission-run.created |
| `23:12:22.311` | `start-team` | `absent -> ready` | store index 5, step-run.state |
| `23:22:22.398` | `start-team` | `working -> failed` | store index 34, step-run.state (the active execution timeout expired) |
| `23:22:22.438` | `process-before-restart` | `absent -> cancelled` | store index 37, step-run.state |
| `23:22:22.438` | `inspect-durable-state` | `absent -> cancelled` | store index 38, step-run.state |
| `23:22:22.438` | `process-item-1` | `absent -> cancelled` | store index 39, step-run.state |
| `23:22:22.439` | `process-item-2` | `absent -> cancelled` | store index 40, step-run.state |
| `23:22:22.439` | `publish-pre-restart-revision` | `absent -> cancelled` | store index 41, step-run.state |
| `23:22:22.439` | `inject-cold-restart` | `absent -> cancelled` | store index 42, step-run.state |
| `23:22:22.440` | `process-after-restart` | `absent -> cancelled` | store index 43, step-run.state |
| `23:22:22.440` | `inspect-recovered-state` | `absent -> cancelled` | store index 44, step-run.state |
| `23:22:22.440` | `process-item-3` | `absent -> cancelled` | store index 45, step-run.state |
| `23:22:22.440` | `process-item-4` | `absent -> cancelled` | store index 46, step-run.state |
| `23:22:22.441` | `verify-complete-batch` | `absent -> cancelled` | store index 47, step-run.state |
| `23:22:22.441` | `publish-batch-revision` | `absent -> cancelled` | store index 48, step-run.state |
| `23:22:22.441` | `report-to-supervisor` | `absent -> cancelled` | store index 49, step-run.state |
| `23:22:22.442` | `verify-and-confirm` | `absent -> cancelled` | store index 50, step-run.state |
| `23:22:22.442` | `inspect-graph-history` | `absent -> cancelled` | store index 51, step-run.state |
| `23:22:22.442` | `verify-ledger-read-only` | `absent -> cancelled` | store index 52, step-run.state |
| `23:22:22.443` | `confirm-requester` | `absent -> cancelled` | store index 53, step-run.state |
| `23:22:22.443` | `held-out-gates` | `absent -> cancelled` | store index 54, step-run.state |
| `23:22:23.576` | `mission-run/restart-sonnet-final-20260926-a` | `running -> failed` | store index 65, mission-run.state |

## Small Talk

- Runtime work messages: `0`
- Direct agent messages: `0`
- Required sequence: `daemon/runtime` assigns `process-before-restart` to rc.dev → items 1 and 2 and the pre-restart product → injector hangs up rc.dev and sends one duplicate → `daemon/runtime` assigns `process-after-restart` → items 3 and 4, batch product, one report to rc.sup → rc.sup verifies and sends one confirmation
- Unexpected or duplicate messages: none; no work was assigned

## Judges

| Judge | Result | Duration | Evidence |
| --- | --- | ---: | --- |
| none reached | `n/a` | `n/a` | the mission stopped before any gate ran |
| terminal input audit | `pass` | `n/a` | `0` `terminal.input.requested` claims in the whole graph |

## Result details

- Products or commits: none beyond the graph records above
- Cleanup: complete
- Notable behavior: `start-team` failed when its 10 minute execution timeout expired: the Claude supervisor's terminal session started at 23:12:22 but never rendered or reported a harness state beyond `starting`, while rc.dev became ready in one second. No st3 work was assigned, and the session exited at teardown.
- Notable behavior: This is a Claude Code startup hang, not an st3 result; the omp and Codex restart runs started at the same time on the same binary brought up their Claude supervisors normally.
- Follow-up: rerun as `restart-sonnet-final-20260926-b`
