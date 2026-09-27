# Eval run report — 2026-09-26

- Eval: `restart-continuity` (rc.dev on Codex `gpt-6-luna`, rc.sup on Claude `claude-sonnet-5`)
- Runtime: `st3`
- Run ID: `mission-run/restart-codex-final-20260926-a`
- Candidate commit: st3 binary built from `f6c76b6`, which also listed boot-contract work with `--as "$ST_AGENT"`; `72c04ce` reverted that listing; eval files with the fixture fix in `8dd9cbb`
- Eval KDL SHA-256: `44e93570e970114795b43c09193b7daea76d00fb193560bf4300c0c6286392bc` (`variants/codex-luna.kdl`)
- Result: `pass`

## Timing

- Started: `2026-09-26T23:12:20.318Z`
- Ended: `2026-09-26T23:17:38.689Z`
- Duration: `318.371`

## Model usage

| Role | Harness | Model | Tokens |
| --- | --- | --- | ---: |
| `rc.dev` | `codex` | `gpt-6-luna × 1 turns` | `552,215` |
| `rc.dev` | `codex` | `gpt-6-luna × 1 turns` | `910,118` |
| `rc.sup` | `claude` | `claude-sonnet-5 × 36 turns` | `1,631,820` |

- Agent tokens: `3,094,153`
- LLM judge tokens: `0` (no LLM judge)
- Total model tokens: `3,094,153`
- Count source: provider transcripts. Models are counted per assistant turn in each transcript. Tokens are omp `usage.totalTokens` summed per turn, Codex final `total_token_usage.total_tokens`, and Claude input + output + cache-read + cache-creation tokens summed per turn; cached reads dominate. The mission projection reported `usage: null`.

## Observable graph transitions

| Time | Subject or step | Transition | Evidence |
| --- | --- | --- | --- |
| `23:12:20.318` | `mission-run/restart-codex-final-20260926-a` | `absent -> created` | store index 3, mission-run.created |
| `23:12:20.336` | `start-team` | `absent -> ready` | store index 5, step-run.state |
| `23:12:22.635` | `start-team` | `working -> completed` | store index 29, step-run.state |
| `23:12:22.683` | `process-before-restart` | `absent -> ready` | store index 30, step-run.state |
| `23:12:22.709` | `message/00fc4c18c54cd8cf` | `wake attempt 1 sent` | store index 31, to rc.dev |
| `23:12:34.348` | `process-before-restart` | `claimed` | store index 40, actor rc.dev |
| `23:12:34.367` | `inspect-durable-state` | `absent -> ready` | store index 41, step-run.state |
| `23:12:45.970` | `inspect-durable-state` | `claimed` | store index 42, actor rc.dev |
| `23:12:55.670` | `inspect-durable-state` | `submitted` | store index 43, actor rc.dev |
| `23:12:55.690` | `inspect-durable-state` | `ready -> completed` | store index 44, step-run.state |
| `23:12:55.730` | `process-item-1` | `absent -> ready` | store index 45, step-run.state |
| `23:13:03.518` | `process-item-1` | `claimed` | store index 47, actor rc.dev |
| … | … | 51 further transitions omitted | `trace.jsonl` |
| `23:17:10.299` | `verify-ledger-read-only` | `claimed` | store index 161, actor rc.sup |
| `23:17:13.592` | `verify-ledger-read-only` | `submitted` | store index 162, actor rc.sup |
| `23:17:13.614` | `verify-ledger-read-only` | `ready -> completed` | store index 163, step-run.state |
| `23:17:13.658` | `confirm-requester` | `absent -> ready` | store index 164, step-run.state |
| `23:17:18.682` | `confirm-requester` | `claimed` | store index 165, actor rc.sup |
| `23:17:30.646` | `confirm-requester` | `submitted` | store index 169, actor rc.sup |
| `23:17:30.701` | `confirm-requester` | `ready -> completed` | store index 170, step-run.state |
| `23:17:36.216` | `verify-and-confirm` | `submitted` | store index 171, actor rc.sup |
| `23:17:36.237` | `verify-and-confirm` | `ready -> completed` | store index 172, step-run.state |
| `23:17:36.282` | `held-out-gates` | `absent -> ready` | store index 173, step-run.state |
| `23:17:38.141` | `held-out-gates` | `working -> completed` | store index 231, step-run.state |
| `23:17:38.689` | `mission-run/restart-codex-final-20260926-a` | `running -> completed` | store index 243, mission-run.state |

## Small Talk

- Runtime work messages: `3` (`process-before-restart` × 1, `process-after-restart` × 1, `verify-and-confirm` × 1)
- Direct agent messages: `3`
- Required sequence: `daemon/runtime` assigns `process-before-restart` to rc.dev → items 1 and 2 and the pre-restart product → injector hangs up rc.dev and sends one duplicate → `daemon/runtime` assigns `process-after-restart` → items 3 and 4, batch product, one report to rc.sup → rc.sup verifies and sends one confirmation
- Unexpected or duplicate messages: none in the judged sequence: two rc.dev assignments, one duplicate, one report, one confirmation

## Judges

| Judge | Result | Duration | Evidence |
| --- | --- | ---: | --- |
| condition-1 | `pass` | `n/a` | stage `start-team` |
| condition-2 | `pass` | `n/a` | stage `start-team` |
| condition-1 | `pass` | `n/a` | stage `inject-cold-restart` |
| condition-2 | `pass` | `n/a` | stage `inject-cold-restart` |
| the cold restart straddles the item commits | `pass` | `0.237` | stage `held-out-gates` |
| every stable item completed exactly once | `pass` | `0.000` | stage `held-out-gates` |
| every item has a durable progress record and working handler | `pass` | `0.000` | stage `held-out-gates` |
| the final artifact is not corrupt | `pass` | `0.347` | stage `held-out-gates` |
| only rc.dev changed the ledger repository | `pass` | `0.202` | stage `held-out-gates` |
| the graph has one product for each stable result | `pass` | `0.000` | stage `held-out-gates` |
| Small Talk has two work assignments, one duplicate, one report, and one confirmation | `pass` | `0.252` | stage `held-out-gates` |
| terminal input audit | `pass` | `n/a` | `0` `terminal.input.requested` claims in the whole graph |

## Result details

- Products or commits: none beyond the graph records above
- Cleanup: complete
- Notable behavior: All seven held-out gates passed. rc.dev and the Claude supervisor both finished every nested step before submitting its parent.
- Follow-up: none
