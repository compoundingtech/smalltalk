# Eval run report — 2026-09-26

- Eval: `restart-continuity` (rc.dev on Codex `gpt-6-luna`, rc.sup on Claude `claude-sonnet-5`)
- Runtime: `st3`
- Run ID: `mission-run/restart-codex-final-20260926-c`
- Candidate commit: st3 binary built from `72c04ce`, the branch's final st3 source; eval files with the fixture fix in `8dd9cbb`
- Eval KDL SHA-256: `44e93570e970114795b43c09193b7daea76d00fb193560bf4300c0c6286392bc` (`variants/codex-luna.kdl`)
- Result: `fail`

## Timing

- Started: `2026-09-26T23:27:18.531Z`
- Ended: `2026-09-26T23:32:20.279Z`
- Duration: `301.748`

## Model usage

| Role | Harness | Model | Tokens |
| --- | --- | --- | ---: |
| `rc.dev` | `codex` | `gpt-6-luna × 1 turns` | `576,707` |
| `rc.dev` | `codex` | `gpt-6-luna × 1 turns` | `785,413` |
| `rc.sup` | `claude` | `claude-sonnet-5 × 32 turns` | `1,431,131` |

- Agent tokens: `2,793,251`
- LLM judge tokens: `0` (no LLM judge)
- Total model tokens: `2,793,251`
- Count source: provider transcripts. Models are counted per assistant turn in each transcript. Tokens are omp `usage.totalTokens` summed per turn, Codex final `total_token_usage.total_tokens`, and Claude input + output + cache-read + cache-creation tokens summed per turn; cached reads dominate. The mission projection reported `usage: null`.

## Observable graph transitions

| Time | Subject or step | Transition | Evidence |
| --- | --- | --- | --- |
| `23:27:18.531` | `mission-run/restart-codex-final-20260926-c` | `absent -> created` | store index 3, mission-run.created |
| `23:27:18.543` | `start-team` | `absent -> ready` | store index 5, step-run.state |
| `23:27:20.809` | `start-team` | `working -> completed` | store index 30, step-run.state |
| `23:27:20.848` | `process-before-restart` | `absent -> ready` | store index 31, step-run.state |
| `23:27:20.869` | `message/b617fa11d41a62d3` | `wake attempt 1 sent` | store index 32, to rc.dev |
| `23:27:30.736` | `process-before-restart` | `claimed` | store index 39, actor rc.dev |
| `23:27:30.754` | `inspect-durable-state` | `absent -> ready` | store index 40, step-run.state |
| `23:28:30.574` | `inspect-durable-state` | `claimed` | store index 43, actor rc.dev |
| `23:28:40.087` | `inspect-durable-state` | `submitted` | store index 44, actor rc.dev |
| `23:28:40.111` | `inspect-durable-state` | `ready -> completed` | store index 45, step-run.state |
| `23:28:40.151` | `process-item-1` | `absent -> ready` | store index 46, step-run.state |
| `23:28:51.908` | `process-item-1` | `claimed` | store index 47, actor rc.dev |
| … | … | 54 further transitions omitted | `trace.jsonl` |
| `23:31:57.243` | `verify-ledger-read-only` | `claimed` | store index 169, actor rc.sup |
| `23:32:01.828` | `verify-ledger-read-only` | `submitted` | store index 171, actor rc.sup |
| `23:32:01.850` | `verify-ledger-read-only` | `ready -> completed` | store index 172, step-run.state |
| `23:32:01.894` | `confirm-requester` | `absent -> ready` | store index 173, step-run.state |
| `23:32:05.150` | `message/7b42f903827e7523` | `wake attempt 1 sent` | store index 175, to rc.sup |
| `23:32:08.332` | `confirm-requester` | `claimed` | store index 179, actor rc.sup |
| `23:32:17.768` | `confirm-requester` | `submitted` | store index 182, actor rc.sup |
| `23:32:17.790` | `confirm-requester` | `ready -> completed` | store index 183, step-run.state |
| `23:32:17.833` | `verify-and-confirm` | `ready -> completed` | store index 184, step-run.state |
| `23:32:17.875` | `held-out-gates` | `absent -> ready` | store index 185, step-run.state |
| `23:32:19.736` | `held-out-gates` | `working -> failed` | store index 243, step-run.state (mechanical gate `Small Talk has two work assignments, one duplicate, one report, and one confirmation` fail) |
| `23:32:20.279` | `mission-run/restart-codex-final-20260926-c` | `running -> failed` | store index 259, mission-run.state |

## Small Talk

- Runtime work messages: `6` (`process-before-restart` × 1, `process-after-restart` × 1, `verify-and-confirm` × 1, `inspect-graph-history` × 1, `verify-ledger-read-only` × 1, `confirm-requester` × 1)
- Direct agent messages: `4`
- Required sequence: `daemon/runtime` assigns `process-before-restart` to rc.dev → items 1 and 2 and the pre-restart product → injector hangs up rc.dev and sends one duplicate → `daemon/runtime` assigns `process-after-restart` → items 3 and 4, batch product, one report to rc.sup → rc.sup verifies and sends one confirmation
- Unexpected or duplicate messages: rc.dev tagged its report to rc.sup `mission-run:mission-run/<run>` instead of the requested `mission-run:<run>`, so the judge found no report. The Claude supervisor submitted `verify-and-confirm` before its nested steps, sent an untagged note to the requester, and later a tagged confirmation that said it superseded the note.

## Judges

| Judge | Result | Duration | Evidence |
| --- | --- | ---: | --- |
| condition-1 | `pass` | `n/a` | stage `start-team` |
| condition-2 | `pass` | `n/a` | stage `start-team` |
| condition-1 | `pass` | `n/a` | stage `inject-cold-restart` |
| condition-2 | `pass` | `n/a` | stage `inject-cold-restart` |
| the cold restart straddles the item commits | `pass` | `0.000` | stage `held-out-gates` |
| every stable item completed exactly once | `pass` | `0.192` | stage `held-out-gates` |
| every item has a durable progress record and working handler | `pass` | `0.234` | stage `held-out-gates` |
| the final artifact is not corrupt | `pass` | `0.000` | stage `held-out-gates` |
| only rc.dev changed the ledger repository | `pass` | `0.201` | stage `held-out-gates` |
| the graph has one product for each stable result | `pass` | `0.000` | stage `held-out-gates` |
| Small Talk has two work assignments, one duplicate, one report, and one confirmation | `fail` | `0.248` | stage `held-out-gates`; mechanical gate `Small Talk has two work assignments, one duplicate, one report, and one confirmation` fail |
| terminal input audit | `pass` | `n/a` | `0` `terminal.input.requested` claims in the whole graph |

## Result details

- Products or commits: none beyond the graph records above
- Cleanup: complete
- Notable behavior: Six of seven held-out gates passed. rc.dev (Codex) completed every nested step in order with two assignment wakes; the coordination gate failed on the report tag above.
- Notable behavior: This is a model slip, not an st3 fault; the same variant passed on `ed38d1f`, `0e0358e`, and `f6c76b6`.
- Follow-up: none for omp
