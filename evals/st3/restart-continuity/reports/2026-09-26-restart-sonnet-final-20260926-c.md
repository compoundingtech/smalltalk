# Eval run report — 2026-09-26

- Eval: `restart-continuity` (committed eval: rc.dev and rc.sup on Claude `claude-sonnet-5`)
- Runtime: `st3`
- Run ID: `mission-run/restart-sonnet-final-20260926-c`
- Candidate commit: st3 binary built from `72c04ce`, whose boot contract carried the explicit claim, mailbox, and nested-order lines that `21a507e` removed; eval files with the fixture fix in `8dd9cbb`
- Eval KDL SHA-256: `0bb121739de7e64385bf37fc6a0bf65dfbbd2c538ce061a668f5f9f743922cb4` (`eval.kdl`)
- Result: `fail`

## Timing

- Started: `2026-09-26T23:44:20.292Z`
- Ended: `2026-09-26T23:48:31.404Z`
- Duration: `251.112`

## Model usage

| Role | Harness | Model | Tokens |
| --- | --- | --- | ---: |
| `rc.dev` | `claude` | `claude-sonnet-5 × 36 turns` | `1,746,790` |
| `rc.dev` | `claude` | `claude-sonnet-5 × 27 turns` | `1,201,734` |
| `rc.sup` | `claude` | `claude-sonnet-5 × 37 turns` | `1,700,449` |

- Agent tokens: `4,648,973`
- LLM judge tokens: `0` (no LLM judge)
- Total model tokens: `4,648,973`
- Count source: provider transcripts. Models are counted per assistant turn in each transcript. Tokens are omp `usage.totalTokens` summed per turn, Codex final `total_token_usage.total_tokens`, and Claude input + output + cache-read + cache-creation tokens summed per turn; cached reads dominate. The mission projection reported `usage: null`.

## Observable graph transitions

| Time | Subject or step | Transition | Evidence |
| --- | --- | --- | --- |
| `23:44:20.292` | `mission-run/restart-sonnet-final-20260926-c` | `absent -> created` | store index 3, mission-run.created |
| `23:44:20.305` | `start-team` | `absent -> ready` | store index 5, step-run.state |
| `23:44:21.905` | `start-team` | `working -> completed` | store index 29, step-run.state |
| `23:44:21.951` | `process-before-restart` | `absent -> ready` | store index 30, step-run.state |
| `23:44:21.974` | `message/3a13c44967342def` | `wake attempt 1 sent` | store index 31, to rc.dev |
| `23:44:27.429` | `process-before-restart` | `claimed` | store index 42, actor rc.dev |
| `23:44:27.451` | `inspect-durable-state` | `absent -> ready` | store index 43, step-run.state |
| `23:44:48.181` | `process-before-restart` | `submitted` | store index 45, actor rc.dev |
| `23:44:52.604` | `message/16c8778821d458d7` | `wake attempt 1 sent` | store index 47, to rc.dev |
| `23:44:57.020` | `inspect-durable-state` | `claimed` | store index 51, actor rc.dev |
| `23:45:02.658` | `inspect-durable-state` | `submitted` | store index 54, actor rc.dev |
| `23:45:02.683` | `inspect-durable-state` | `ready -> completed` | store index 55, step-run.state |
| … | … | 64 further transitions omitted | `trace.jsonl` |
| `23:48:09.171` | `verify-ledger-read-only` | `claimed` | store index 230, actor rc.sup |
| `23:48:13.568` | `verify-ledger-read-only` | `submitted` | store index 231, actor rc.sup |
| `23:48:13.590` | `verify-ledger-read-only` | `ready -> completed` | store index 232, step-run.state |
| `23:48:13.638` | `confirm-requester` | `absent -> ready` | store index 233, step-run.state |
| `23:48:16.950` | `message/bd9faa14c7df0678` | `wake attempt 1 sent` | store index 235, to rc.sup |
| `23:48:20.162` | `confirm-requester` | `claimed` | store index 239, actor rc.sup |
| `23:48:28.621` | `confirm-requester` | `submitted` | store index 243, actor rc.sup |
| `23:48:28.644` | `confirm-requester` | `ready -> completed` | store index 244, step-run.state |
| `23:48:28.694` | `verify-and-confirm` | `ready -> completed` | store index 245, step-run.state |
| `23:48:28.741` | `held-out-gates` | `absent -> ready` | store index 246, step-run.state |
| `23:48:30.601` | `held-out-gates` | `working -> failed` | store index 304, step-run.state (mechanical gate `Small Talk has two work assignments, one duplicate, one report, and one confirmation` fail) |
| `23:48:31.404` | `mission-run/restart-sonnet-final-20260926-c` | `running -> failed` | store index 318, mission-run.state |

## Small Talk

- Runtime work messages: `16` (`process-before-restart` × 1, `inspect-durable-state` × 1, `process-item-1` × 1, `process-item-2` × 1, `publish-pre-restart-revision` × 1, `process-after-restart` × 1, `inspect-recovered-state` × 1, `process-item-3` × 1, `process-item-4` × 1, `verify-complete-batch` × 1, `publish-batch-revision` × 1, `report-to-supervisor` × 1, `verify-and-confirm` × 1, `inspect-graph-history` × 1, `verify-ledger-read-only` × 1, `confirm-requester` × 1)
- Direct agent messages: `4`
- Required sequence: `daemon/runtime` assigns `process-before-restart` to rc.dev → items 1 and 2 and the pre-restart product → injector hangs up rc.dev and sends one duplicate → `daemon/runtime` assigns `process-after-restart` → items 3 and 4, batch product, one report to rc.sup → rc.sup verifies and sends one confirmation
- Unexpected or duplicate messages: 12 runtime work messages to rc.dev instead of 2: both Claude seats submitted every parent before any nested step and ended the turn, so each nested step needed its own wake.

## Judges

| Judge | Result | Duration | Evidence |
| --- | --- | ---: | --- |
| condition-1 | `pass` | `n/a` | stage `start-team` |
| condition-2 | `pass` | `n/a` | stage `start-team` |
| condition-1 | `pass` | `n/a` | stage `inject-cold-restart` |
| condition-2 | `pass` | `n/a` | stage `inject-cold-restart` |
| the cold restart straddles the item commits | `pass` | `0.000` | stage `held-out-gates` |
| every stable item completed exactly once | `pass` | `0.000` | stage `held-out-gates` |
| every item has a durable progress record and working handler | `pass` | `0.000` | stage `held-out-gates` |
| the final artifact is not corrupt | `pass` | `0.000` | stage `held-out-gates` |
| only rc.dev changed the ledger repository | `pass` | `0.000` | stage `held-out-gates` |
| the graph has one product for each stable result | `pass` | `0.000` | stage `held-out-gates` |
| Small Talk has two work assignments, one duplicate, one report, and one confirmation | `fail` | `0.199` | stage `held-out-gates`; mechanical gate `Small Talk has two work assignments, one duplicate, one report, and one confirmation` fail |
| terminal input audit | `pass` | `n/a` | `0` `terminal.input.requested` claims in the whole graph |

## Result details

- Products or commits: none beyond the graph records above
- Cleanup: complete
- Notable behavior: Six of seven held-out gates passed; the coordination gate failed on the extra runtime work messages. All work, products, and the ledger were correct.
- Notable behavior: The Claude sonnet worker has submitted each parent before any nested step in every restart attempt.
- Follow-up: none for omp
