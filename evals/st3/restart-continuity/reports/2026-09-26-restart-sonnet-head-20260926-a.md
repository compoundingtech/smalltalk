# Eval run report — 2026-09-26

- Eval: `restart-continuity` (committed eval: rc.dev and rc.sup on Claude `claude-sonnet-5`)
- Runtime: `st3`
- Run ID: `mission-run/restart-sonnet-head-20260926-a`
- Candidate commit: st3 binary built from `21a507e` (baseline boot contract restored); `c8288a8` later changed only omp's delivery mode; eval files with the fixture fix in `8dd9cbb`
- Eval KDL SHA-256: `0bb121739de7e64385bf37fc6a0bf65dfbbd2c538ce061a668f5f9f743922cb4` (`eval.kdl`)
- Result: `fail`

## Timing

- Started: `2026-09-26T23:58:32.807Z`
- Ended: `2026-09-27T00:02:05.369Z`
- Duration: `212.562`

## Model usage

| Role | Harness | Model | Tokens |
| --- | --- | --- | ---: |
| `rc.dev` | `claude` | `claude-sonnet-5 × 21 turns` | `944,691` |
| `rc.dev` | `claude` | `claude-sonnet-5 × 26 turns` | `1,144,116` |
| `rc.sup` | `claude` | `claude-sonnet-5 × 30 turns` | `1,343,745` |

- Agent tokens: `3,432,552`
- LLM judge tokens: `0` (no LLM judge)
- Total model tokens: `3,432,552`
- Count source: provider transcripts. Models are counted per assistant turn in each transcript. Tokens are omp `usage.totalTokens` summed per turn, Codex final `total_token_usage.total_tokens`, and Claude input + output + cache-read + cache-creation tokens summed per turn; cached reads dominate. The mission projection reported `usage: null`.

## Observable graph transitions

| Time | Subject or step | Transition | Evidence |
| --- | --- | --- | --- |
| `23:58:32.807` | `mission-run/restart-sonnet-head-20260926-a` | `absent -> created` | store index 3, mission-run.created |
| `23:58:32.821` | `start-team` | `absent -> ready` | store index 5, step-run.state |
| `23:58:34.441` | `start-team` | `working -> completed` | store index 29, step-run.state |
| `23:58:34.491` | `process-before-restart` | `absent -> ready` | store index 30, step-run.state |
| `23:58:34.513` | `message/c0221009a9e0433c` | `wake attempt 1 sent` | store index 31, to rc.dev |
| `23:58:41.758` | `process-before-restart` | `claimed` | store index 42, actor rc.dev |
| `23:58:41.781` | `inspect-durable-state` | `absent -> ready` | store index 43, step-run.state |
| `23:58:59.014` | `process-before-restart` | `submitted` | store index 45, actor rc.dev |
| `23:59:02.152` | `message/323912c3df14fdad` | `wake attempt 1 sent` | store index 47, to rc.dev |
| `23:59:06.233` | `inspect-durable-state` | `claimed` | store index 51, actor rc.dev |
| `23:59:11.598` | `inspect-durable-state` | `submitted` | store index 52, actor rc.dev |
| `23:59:11.619` | `inspect-durable-state` | `ready -> completed` | store index 53, step-run.state |
| … | … | 55 further transitions omitted | `trace.jsonl` |
| `00:01:37.915` | `verify-ledger-read-only` | `claimed` | store index 180, actor rc.sup |
| `00:01:40.536` | `verify-ledger-read-only` | `submitted` | store index 181, actor rc.sup |
| `00:01:40.559` | `verify-ledger-read-only` | `ready -> completed` | store index 182, step-run.state |
| `00:01:40.603` | `confirm-requester` | `absent -> ready` | store index 183, step-run.state |
| `00:01:45.640` | `confirm-requester` | `claimed` | store index 184, actor rc.sup |
| `00:01:56.601` | `confirm-requester` | `submitted` | store index 187, actor rc.sup |
| `00:01:56.658` | `confirm-requester` | `ready -> completed` | store index 188, step-run.state |
| `00:02:02.505` | `verify-and-confirm` | `submitted` | store index 190, actor rc.sup |
| `00:02:02.527` | `verify-and-confirm` | `ready -> completed` | store index 191, step-run.state |
| `00:02:02.572` | `held-out-gates` | `absent -> ready` | store index 192, step-run.state |
| `00:02:04.461` | `held-out-gates` | `working -> failed` | store index 250, step-run.state (mechanical gate `Small Talk has two work assignments, one duplicate, one report, and one confirmation` fail) |
| `00:02:05.369` | `mission-run/restart-sonnet-head-20260926-a` | `running -> failed` | store index 265, mission-run.state |

## Small Talk

- Runtime work messages: `7` (`process-before-restart` × 1, `inspect-durable-state` × 1, `process-item-1` × 1, `process-item-2` × 1, `publish-pre-restart-revision` × 1, `process-after-restart` × 1, `verify-and-confirm` × 1)
- Direct agent messages: `2`
- Required sequence: `daemon/runtime` assigns `process-before-restart` to rc.dev → items 1 and 2 and the pre-restart product → injector hangs up rc.dev and sends one duplicate → `daemon/runtime` assigns `process-after-restart` → items 3 and 4, batch product, one report to rc.sup → rc.sup verifies and sends one confirmation
- Unexpected or duplicate messages: Extra runtime work messages to rc.dev: the Claude worker submitted `process-before-restart` before its nested steps and ended the turn, so each of those four nested steps needed its own wake.

## Judges

| Judge | Result | Duration | Evidence |
| --- | --- | ---: | --- |
| condition-1 | `pass` | `n/a` | stage `start-team` |
| condition-2 | `pass` | `n/a` | stage `start-team` |
| condition-1 | `pass` | `n/a` | stage `inject-cold-restart` |
| condition-2 | `pass` | `n/a` | stage `inject-cold-restart` |
| the cold restart straddles the item commits | `pass` | `0.000` | stage `held-out-gates` |
| every stable item completed exactly once | `pass` | `0.206` | stage `held-out-gates` |
| every item has a durable progress record and working handler | `pass` | `0.240` | stage `held-out-gates` |
| the final artifact is not corrupt | `pass` | `0.000` | stage `held-out-gates` |
| only rc.dev changed the ledger repository | `pass` | `0.202` | stage `held-out-gates` |
| the graph has one product for each stable result | `pass` | `0.000` | stage `held-out-gates` |
| Small Talk has two work assignments, one duplicate, one report, and one confirmation | `fail` | `0.241` | stage `held-out-gates`; mechanical gate `Small Talk has two work assignments, one duplicate, one report, and one confirmation` fail |
| terminal input audit | `pass` | `n/a` | `0` `terminal.input.requested` claims in the whole graph |

## Result details

- Products or commits: none beyond the graph records above
- Cleanup: complete
- Notable behavior: Six of seven held-out gates passed; the coordination gate failed on the assignment count. rc.dev completed `process-after-restart` in order this time.
- Follow-up: none for omp
