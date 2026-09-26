# Eval run report — 2026-09-26

- Eval: `restart-continuity` (committed eval: rc.dev and rc.sup on Claude `claude-sonnet-5`)
- Runtime: `st3`
- Run ID: `mission-run/restart-sonnet-final-20260926-b`
- Candidate commit: st3 binary built from `72c04ce`, the branch's final st3 source; eval files with the fixture fix in `8dd9cbb`
- Eval KDL SHA-256: `0bb121739de7e64385bf37fc6a0bf65dfbbd2c538ce061a668f5f9f743922cb4` (`eval.kdl`)
- Result: `fail`

## Timing

- Started: `2026-09-26T23:27:21.568Z`
- Ended: `2026-09-26T23:31:25.266Z`
- Duration: `243.698`

## Model usage

| Role | Harness | Model | Tokens |
| --- | --- | --- | ---: |
| `rc.dev` | `claude` | `claude-sonnet-5 × 27 turns` | `1,188,707` |
| `rc.dev` | `claude` | `claude-sonnet-5 × 38 turns` | `1,800,362` |
| `rc.sup` | `claude` | `claude-sonnet-5 × 40 turns` | `1,848,078` |

- Agent tokens: `4,837,147`
- LLM judge tokens: `0` (no LLM judge)
- Total model tokens: `4,837,147`
- Count source: provider transcripts. Models are counted per assistant turn in each transcript. Tokens are omp `usage.totalTokens` summed per turn, Codex final `total_token_usage.total_tokens`, and Claude input + output + cache-read + cache-creation tokens summed per turn; cached reads dominate. The mission projection reported `usage: null`.

## Observable graph transitions

| Time | Subject or step | Transition | Evidence |
| --- | --- | --- | --- |
| `23:27:21.568` | `mission-run/restart-sonnet-final-20260926-b` | `absent -> created` | store index 3, mission-run.created |
| `23:27:21.581` | `start-team` | `absent -> ready` | store index 5, step-run.state |
| `23:27:23.174` | `start-team` | `working -> completed` | store index 30, step-run.state |
| `23:27:23.215` | `process-before-restart` | `absent -> ready` | store index 31, step-run.state |
| `23:27:23.236` | `message/ca05d05a39e29f6d` | `wake attempt 1 sent` | store index 32, to rc.dev |
| `23:27:27.864` | `process-before-restart` | `claimed` | store index 42, actor rc.dev |
| `23:27:27.884` | `inspect-durable-state` | `absent -> ready` | store index 43, step-run.state |
| `23:27:44.741` | `process-before-restart` | `submitted` | store index 45, actor rc.dev |
| `23:27:47.899` | `message/f837b8cc02359c9b` | `wake attempt 1 sent` | store index 47, to rc.dev |
| `23:27:51.908` | `inspect-durable-state` | `claimed` | store index 51, actor rc.dev |
| `23:27:57.903` | `inspect-durable-state` | `submitted` | store index 52, actor rc.dev |
| `23:27:57.923` | `inspect-durable-state` | `ready -> completed` | store index 53, step-run.state |
| … | … | 61 further transitions omitted | `trace.jsonl` |
| `23:30:50.281` | `verify-ledger-read-only` | `claimed` | store index 215, actor rc.sup |
| `23:30:55.666` | `verify-ledger-read-only` | `submitted` | store index 216, actor rc.sup |
| `23:30:55.694` | `verify-ledger-read-only` | `ready -> completed` | store index 217, step-run.state |
| `23:30:55.743` | `confirm-requester` | `absent -> ready` | store index 218, step-run.state |
| `23:31:02.519` | `confirm-requester` | `claimed` | store index 220, actor rc.sup |
| `23:31:13.612` | `confirm-requester` | `submitted` | store index 223, actor rc.sup |
| `23:31:13.673` | `confirm-requester` | `ready -> completed` | store index 224, step-run.state |
| `23:31:22.420` | `verify-and-confirm` | `submitted` | store index 225, actor rc.sup |
| `23:31:22.443` | `verify-and-confirm` | `ready -> completed` | store index 226, step-run.state |
| `23:31:22.494` | `held-out-gates` | `absent -> ready` | store index 227, step-run.state |
| `23:31:24.464` | `held-out-gates` | `working -> failed` | store index 285, step-run.state (mechanical gate `Small Talk has two work assignments, one duplicate, one report, and one confirmation` fail) |
| `23:31:25.266` | `mission-run/restart-sonnet-final-20260926-b` | `running -> failed` | store index 299, mission-run.state |

## Small Talk

- Runtime work messages: `13` (`process-before-restart` × 1, `inspect-durable-state` × 1, `process-item-1` × 1, `process-item-2` × 1, `publish-pre-restart-revision` × 1, `process-after-restart` × 1, `inspect-recovered-state` × 1, `process-item-3` × 1, `process-item-4` × 1, `verify-complete-batch` × 1, `publish-batch-revision` × 1, `report-to-supervisor` × 1, `verify-and-confirm` × 1)
- Direct agent messages: `3`
- Required sequence: `daemon/runtime` assigns `process-before-restart` to rc.dev → items 1 and 2 and the pre-restart product → injector hangs up rc.dev and sends one duplicate → `daemon/runtime` assigns `process-after-restart` → items 3 and 4, batch product, one report to rc.sup → rc.sup verifies and sends one confirmation
- Unexpected or duplicate messages: 12 runtime work messages to rc.dev instead of 2: the Claude worker submitted both parents before any nested step and ended its turn each time, so each nested step needed its own wake.

## Judges

| Judge | Result | Duration | Evidence |
| --- | --- | ---: | --- |
| condition-1 | `pass` | `n/a` | stage `start-team` |
| condition-2 | `pass` | `n/a` | stage `start-team` |
| condition-1 | `pass` | `n/a` | stage `inject-cold-restart` |
| condition-2 | `pass` | `n/a` | stage `inject-cold-restart` |
| the cold restart straddles the item commits | `pass` | `0.260` | stage `held-out-gates` |
| every stable item completed exactly once | `pass` | `0.000` | stage `held-out-gates` |
| every item has a durable progress record and working handler | `pass` | `0.000` | stage `held-out-gates` |
| the final artifact is not corrupt | `pass` | `0.367` | stage `held-out-gates` |
| only rc.dev changed the ledger repository | `pass` | `0.196` | stage `held-out-gates` |
| the graph has one product for each stable result | `pass` | `0.359` | stage `held-out-gates` |
| Small Talk has two work assignments, one duplicate, one report, and one confirmation | `fail` | `0.260` | stage `held-out-gates`; mechanical gate `Small Talk has two work assignments, one duplicate, one report, and one confirmation` fail |
| terminal input audit | `pass` | `n/a` | `0` `terminal.input.requested` claims in the whole graph |

## Result details

- Products or commits: none beyond the graph records above
- Cleanup: complete
- Notable behavior: Six of seven held-out gates passed; the coordination gate failed on the assignment count. The boot contract's nested-order sentence did not change the Claude worker's early parent submissions, though the Claude supervisor worked its nested steps in order.
- Notable behavior: The Claude sonnet worker has now submitted its parent before any nested step in every restart attempt.
- Follow-up: none for omp
