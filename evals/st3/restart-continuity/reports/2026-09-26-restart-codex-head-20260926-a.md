# Eval run report — 2026-09-26

- Eval: `restart-continuity` (rc.dev on Codex `gpt-6-luna`, rc.sup on Claude `claude-sonnet-5`)
- Runtime: `st3`
- Run ID: `mission-run/restart-codex-head-20260926-a`
- Candidate commit: st3 binary built from `21a507e` (baseline boot contract restored); `c8288a8` later changed only omp's delivery mode; eval files with the fixture fix in `8dd9cbb`
- Eval KDL SHA-256: `44e93570e970114795b43c09193b7daea76d00fb193560bf4300c0c6286392bc` (`variants/codex-luna.kdl`)
- Result: `pass`

## Timing

- Started: `2026-09-26T23:58:29.826Z`
- Ended: `2026-09-27T00:06:45.670Z`
- Duration: `495.844`

## Model usage

| Role | Harness | Model | Tokens |
| --- | --- | --- | ---: |
| `rc.dev` | `codex` | `gpt-6-luna × 1 turns` | `609,786` |
| `rc.dev` | `codex` | `gpt-6-luna × 1 turns` | `794,467` |
| `rc.sup` | `claude` | `claude-sonnet-5 × 34 turns` | `1,529,317` |

- Agent tokens: `2,933,570`
- LLM judge tokens: `0` (no LLM judge)
- Total model tokens: `2,933,570`
- Count source: provider transcripts. Models are counted per assistant turn in each transcript. Tokens are omp `usage.totalTokens` summed per turn, Codex final `total_token_usage.total_tokens`, and Claude input + output + cache-read + cache-creation tokens summed per turn; cached reads dominate. The mission projection reported `usage: null`.

## Observable graph transitions

| Time | Subject or step | Transition | Evidence |
| --- | --- | --- | --- |
| `23:58:29.826` | `mission-run/restart-codex-head-20260926-a` | `absent -> created` | store index 3, mission-run.created |
| `23:58:29.839` | `start-team` | `absent -> ready` | store index 5, step-run.state |
| `23:58:32.107` | `start-team` | `working -> completed` | store index 29, step-run.state |
| `23:58:32.151` | `process-before-restart` | `absent -> ready` | store index 30, step-run.state |
| `23:58:32.173` | `message/1d6cad3fbb785020` | `wake attempt 1 sent` | store index 31, to rc.dev |
| `23:58:41.722` | `process-before-restart` | `claimed` | store index 39, actor rc.dev |
| `23:58:41.742` | `inspect-durable-state` | `absent -> ready` | store index 40, step-run.state |
| `23:59:16.672` | `inspect-durable-state` | `claimed` | store index 43, actor rc.dev |
| `23:59:21.622` | `inspect-durable-state` | `submitted` | store index 44, actor rc.dev |
| `23:59:21.645` | `inspect-durable-state` | `ready -> completed` | store index 45, step-run.state |
| `23:59:21.689` | `process-item-1` | `absent -> ready` | store index 46, step-run.state |
| `23:59:28.751` | `process-item-1` | `claimed` | store index 47, actor rc.dev |
| … | … | 54 further transitions omitted | `trace.jsonl` |
| `00:06:19.244` | `verify-ledger-read-only` | `claimed` | store index 167, actor rc.sup |
| `00:06:24.361` | `verify-ledger-read-only` | `submitted` | store index 168, actor rc.sup |
| `00:06:24.384` | `verify-ledger-read-only` | `ready -> completed` | store index 169, step-run.state |
| `00:06:24.428` | `confirm-requester` | `absent -> ready` | store index 170, step-run.state |
| `00:06:27.494` | `message/ae800b04e0e73678` | `wake attempt 1 sent` | store index 172, to rc.sup |
| `00:06:30.835` | `confirm-requester` | `claimed` | store index 176, actor rc.sup |
| `00:06:43.120` | `confirm-requester` | `submitted` | store index 179, actor rc.sup |
| `00:06:43.144` | `confirm-requester` | `ready -> completed` | store index 180, step-run.state |
| `00:06:43.189` | `verify-and-confirm` | `ready -> completed` | store index 181, step-run.state |
| `00:06:43.230` | `held-out-gates` | `absent -> ready` | store index 182, step-run.state |
| `00:06:45.080` | `held-out-gates` | `working -> completed` | store index 240, step-run.state |
| `00:06:45.670` | `mission-run/restart-codex-head-20260926-a` | `running -> completed` | store index 253, mission-run.state |

## Small Talk

- Runtime work messages: `6` (`process-before-restart` × 1, `process-after-restart` × 1, `verify-and-confirm` × 1, `inspect-graph-history` × 1, `verify-ledger-read-only` × 1, `confirm-requester` × 1)
- Direct agent messages: `3`
- Required sequence: `daemon/runtime` assigns `process-before-restart` to rc.dev → items 1 and 2 and the pre-restart product → injector hangs up rc.dev and sends one duplicate → `daemon/runtime` assigns `process-after-restart` → items 3 and 4, batch product, one report to rc.sup → rc.sup verifies and sends one confirmation
- Unexpected or duplicate messages: none in the judged sequence: two rc.dev assignments, one closed duplicate, one report, one confirmation

## Judges

| Judge | Result | Duration | Evidence |
| --- | --- | ---: | --- |
| condition-1 | `pass` | `n/a` | stage `start-team` |
| condition-2 | `pass` | `n/a` | stage `start-team` |
| condition-1 | `pass` | `n/a` | stage `inject-cold-restart` |
| condition-2 | `pass` | `n/a` | stage `inject-cold-restart` |
| the cold restart straddles the item commits | `pass` | `0.000` | stage `held-out-gates` |
| every stable item completed exactly once | `pass` | `0.000` | stage `held-out-gates` |
| every item has a durable progress record and working handler | `pass` | `0.243` | stage `held-out-gates` |
| the final artifact is not corrupt | `pass` | `0.000` | stage `held-out-gates` |
| only rc.dev changed the ledger repository | `pass` | `0.191` | stage `held-out-gates` |
| the graph has one product for each stable result | `pass` | `0.342` | stage `held-out-gates` |
| Small Talk has two work assignments, one duplicate, one report, and one confirmation | `pass` | `0.238` | stage `held-out-gates` |
| terminal input audit | `pass` | `n/a` | `0` `terminal.input.requested` claims in the whole graph |

## Result details

- Products or commits: none beyond the graph records above
- Cleanup: complete
- Notable behavior: All seven held-out gates passed. rc.dev completed every nested step in order; the Claude supervisor submitted its parent first and received one wake per nested step after its turn ended.
- Follow-up: none
