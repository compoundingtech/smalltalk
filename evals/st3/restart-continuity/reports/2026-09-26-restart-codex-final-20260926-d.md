# Eval run report — 2026-09-26

- Eval: `restart-continuity` (rc.dev on Codex `gpt-6-luna`, rc.sup on Claude `claude-sonnet-5`)
- Runtime: `st3`
- Run ID: `mission-run/restart-codex-final-20260926-d`
- Candidate commit: st3 binary built from `72c04ce`, whose boot contract carried the explicit claim, mailbox, and nested-order lines that `21a507e` removed; eval files with the fixture fix in `8dd9cbb`
- Eval KDL SHA-256: `44e93570e970114795b43c09193b7daea76d00fb193560bf4300c0c6286392bc` (`variants/codex-luna.kdl`)
- Result: `pass`

## Timing

- Started: `2026-09-26T23:44:17.340Z`
- Ended: `2026-09-26T23:49:09.922Z`
- Duration: `292.582`

## Model usage

| Role | Harness | Model | Tokens |
| --- | --- | --- | ---: |
| `rc.dev` | `codex` | `gpt-6-luna × 1 turns` | `528,334` |
| `rc.dev` | `codex` | `gpt-6-luna × 1 turns` | `769,820` |
| `rc.sup` | `claude` | `claude-sonnet-5 × 33 turns` | `1,517,314` |

- Agent tokens: `2,815,468`
- LLM judge tokens: `0` (no LLM judge)
- Total model tokens: `2,815,468`
- Count source: provider transcripts. Models are counted per assistant turn in each transcript. Tokens are omp `usage.totalTokens` summed per turn, Codex final `total_token_usage.total_tokens`, and Claude input + output + cache-read + cache-creation tokens summed per turn; cached reads dominate. The mission projection reported `usage: null`.

## Observable graph transitions

| Time | Subject or step | Transition | Evidence |
| --- | --- | --- | --- |
| `23:44:17.340` | `mission-run/restart-codex-final-20260926-d` | `absent -> created` | store index 3, mission-run.created |
| `23:44:17.353` | `start-team` | `absent -> ready` | store index 5, step-run.state |
| `23:44:19.622` | `start-team` | `working -> completed` | store index 29, step-run.state |
| `23:44:19.670` | `process-before-restart` | `absent -> ready` | store index 30, step-run.state |
| `23:44:19.699` | `message/68cce8b6e0b63ba2` | `wake attempt 1 sent` | store index 31, to rc.dev |
| `23:44:35.007` | `process-before-restart` | `claimed` | store index 40, actor rc.dev |
| `23:44:35.026` | `inspect-durable-state` | `absent -> ready` | store index 41, step-run.state |
| `23:45:16.794` | `inspect-durable-state` | `claimed` | store index 43, actor rc.dev |
| `23:45:25.694` | `inspect-durable-state` | `submitted` | store index 44, actor rc.dev |
| `23:45:25.714` | `inspect-durable-state` | `ready -> completed` | store index 45, step-run.state |
| `23:45:25.756` | `process-item-1` | `absent -> ready` | store index 46, step-run.state |
| `23:45:33.821` | `process-item-1` | `claimed` | store index 47, actor rc.dev |
| … | … | 54 further transitions omitted | `trace.jsonl` |
| `23:48:47.603` | `verify-ledger-read-only` | `claimed` | store index 172, actor rc.sup |
| `23:48:49.844` | `verify-ledger-read-only` | `submitted` | store index 173, actor rc.sup |
| `23:48:49.865` | `verify-ledger-read-only` | `ready -> completed` | store index 174, step-run.state |
| `23:48:49.911` | `confirm-requester` | `absent -> ready` | store index 175, step-run.state |
| `23:48:52.983` | `message/e9e37047fa55d36b` | `wake attempt 1 sent` | store index 177, to rc.sup |
| `23:48:56.198` | `confirm-requester` | `claimed` | store index 181, actor rc.sup |
| `23:49:07.243` | `confirm-requester` | `submitted` | store index 185, actor rc.sup |
| `23:49:07.299` | `confirm-requester` | `ready -> completed` | store index 186, step-run.state |
| `23:49:07.343` | `verify-and-confirm` | `ready -> completed` | store index 187, step-run.state |
| `23:49:07.385` | `held-out-gates` | `absent -> ready` | store index 188, step-run.state |
| `23:49:09.331` | `held-out-gates` | `working -> completed` | store index 246, step-run.state |
| `23:49:09.922` | `mission-run/restart-codex-final-20260926-d` | `running -> completed` | store index 259, mission-run.state |

## Small Talk

- Runtime work messages: `6` (`process-before-restart` × 1, `process-after-restart` × 1, `verify-and-confirm` × 1, `inspect-graph-history` × 1, `verify-ledger-read-only` × 1, `confirm-requester` × 1)
- Direct agent messages: `4`
- Required sequence: `daemon/runtime` assigns `process-before-restart` to rc.dev → items 1 and 2 and the pre-restart product → injector hangs up rc.dev and sends one duplicate → `daemon/runtime` assigns `process-after-restart` → items 3 and 4, batch product, one report to rc.sup → rc.sup verifies and sends one confirmation
- Unexpected or duplicate messages: none in the judged sequence: two rc.dev assignments, one closed duplicate, one report, one confirmation

## Judges

| Judge | Result | Duration | Evidence |
| --- | --- | ---: | --- |
| condition-1 | `pass` | `n/a` | stage `start-team` |
| condition-2 | `pass` | `n/a` | stage `start-team` |
| condition-1 | `pass` | `n/a` | stage `inject-cold-restart` |
| condition-2 | `pass` | `n/a` | stage `inject-cold-restart` |
| the cold restart straddles the item commits | `pass` | `0.234` | stage `held-out-gates` |
| every stable item completed exactly once | `pass` | `0.191` | stage `held-out-gates` |
| every item has a durable progress record and working handler | `pass` | `0.242` | stage `held-out-gates` |
| the final artifact is not corrupt | `pass` | `0.357` | stage `held-out-gates` |
| only rc.dev changed the ledger repository | `pass` | `0.209` | stage `held-out-gates` |
| the graph has one product for each stable result | `pass` | `0.412` | stage `held-out-gates` |
| Small Talk has two work assignments, one duplicate, one report, and one confirmation | `pass` | `0.252` | stage `held-out-gates` |
| terminal input audit | `pass` | `n/a` | `0` `terminal.input.requested` claims in the whole graph |

## Result details

- Products or commits: none beyond the graph records above
- Cleanup: complete
- Notable behavior: All seven held-out gates passed. rc.dev completed every nested step in order; the Claude supervisor submitted its parent first and received one wake for each nested step after its turn ended.
- Follow-up: none
