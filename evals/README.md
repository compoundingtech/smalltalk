# Local eval corpus

This repository keeps an st2 and an st3 form of each migration eval.

An st3-only eval can prove a new runtime feature that st2 does not implement.

The initial st2 corpus came from `compoundingtech/evals` commit `3db48ab56d40ce27dfd94f89d2db9b692d93836a`.

Future imports require an explicit eval review before they enter this corpus.

The [source eval migration review](./MIGRATION-REVIEW.md) classifies all 58 active evals in the old eval repository.

| Eval | st2 | st3 |
| --- | --- | --- |
| License MIT | `st2/license-mit` | `st3/license-mit` |
| Ghost bug | `st2/ghost-bug` | `st3/ghost-bug` |
| Signal rename | `st2/signal-rename` | `st3/signal-rename` |
| Restart continuity | `st2/restart-continuity` | `st3/restart-continuity` |
| Fork in the road | `st2/fork-in-the-road` | `st3/fork-in-the-road` |
| Poisoned pull request | `st2/poisoned-pr` | `st3/poisoned-pr` |
| Test Writing | `st2/test-writing` | `st3/test-writing` |
| Weird Git Setup | `st2/weird-git-setup` | `st3/weird-git-setup` |
| Claude Skill Inheritance | `st2/claude-skill-inheritance` | `st3/claude-skill-inheritance` |
| Resource cold start | `st2/resource-cold-start` | `st3/resource-cold-start` |
| Resource retarget | `st2/resource-retarget` | `st3/resource-retarget` |
| Resource handoff | `st2/resource-handoff` | `st3/resource-handoff` |
| Context and resource continuity | `st2/context-resource-continuity` | `st3/context-resource-continuity` |
| Crash escalation | `st2/crash-escalation` | `st3/crash-escalation` |
| PTY attach machine stream | `st2/pty-attach-machine-stream` | `st3/pty-attach-machine-stream` |
| PTY attach only | `st2/pty-attach-only` | `st3/pty-attach-only` |
| PTY send and peek | `st2/pty-send-peek` | `st3/pty-send-peek` |
| Network smoke | `st2/network-smoke` | `st3/network-smoke` |
| Network isolation | `st2/network-isolation` | `st3/network-isolation` |
| Mission Document Lift | Not supported | `st3/mission-document-lift` |
| Mixed Worker Pool | Not supported | `st3/mixed-worker-pool` |
| Cross-harness message wake | Not supported | `st3/cross-harness-message-wake` |
| Work wake reliability | Not supported | `st3/work-wake-reliability` |
| Planning Mode | Not supported | `st3/planning-mode` |
| Run Generation Revision | Not supported | `st3/run-generation-revision` |
| Mission Inputs | Not supported | `st3/mission-inputs` |
| Local File Refresh | Not supported | `st3/local-file-refresh` |
| Constraint Inheritance | Not supported | `st3/constraint-inheritance` |
| Continuous Stewardship | Not supported | `st3/continuous-stewardship` |
| Agent Migration Rehearsal | Not supported | `st3/agent-migration-rehearsal` |
| Automatic GitHub Intake | Not supported | `st3/automatic-github-intake` |
| Codex compaction message delivery | Not supported | `st3/codex-compaction-message-delivery` |
| Native session import | Not supported | `st3/native-session-import` |

The License MIT, Restart continuity, and Claude Skill Inheritance pairs use Claude Sonnet in both runtimes.

The Ghost bug, Signal rename, Fork in the road, Poisoned pull request, Test Writing, and Weird Git Setup pairs use Codex.

The ten remaining pairs are model-free.

Mission Inputs, Local File Refresh, and Constraint Inheritance are also model-free.

Run Generation Revision starts one Codex planner through the launch API.

The st3 corpus has 43 active evals. Twenty-two are model-free, and twenty-one use at least one model.

Each eval KDL starts with a document version. A missing version means version zero.

st2 accepts versions zero and one. st3 accepts version two.

## Harness inventory

The seat counts include every native agent seat. The LLM judge counts are separate.

| Runtime | Eval | Native agent seats | LLM judges |
| --- | --- | --- | --- |
| st2 | License MIT | Claude Sonnet × 2, Codex × 1 | None |
| st2 | Ghost bug | Codex × 2 | None |
| st2 | Signal rename | Codex × 4 | None |
| st2 | Restart continuity | Claude Sonnet × 2 | None |
| st3 | License MIT | Claude Sonnet × 2 | Codex × 1 |
| st3 | Ghost bug | Codex × 2 | Codex × 1 |
| st3 | Signal rename | Codex × 4 | Codex × 1 |
| st3 | Restart continuity | Claude Sonnet × 2 | None |
| st2 | Fork in the road | Codex × 4 | None |
| st3 | Fork in the road | Codex × 4 | None |
| st2 | Poisoned pull request | Codex × 2 | None |
| st3 | Poisoned pull request | Codex × 2 | None |
| st2 | Test Writing | Codex × 2 | None |
| st3 | Test Writing | Codex × 2 | None |
| st2 | Weird Git Setup | Codex × 1 | None |
| st3 | Weird Git Setup | Codex × 1 | None |
| st2 | Claude Skill Inheritance | Claude Sonnet × 1 | None |
| st3 | Claude Skill Inheritance | Claude Sonnet × 1 | None |
| st3 | Mission Document Lift | Codex × 1 | None |
| st3 | Mixed Worker Pool | Claude Sonnet × 1, Codex × 1 | None |
| st3 | Cross-harness message wake | Claude Opus × 1, Codex × 1, Pi × 1, OMP × 1 | None |
| st3 | Work wake reliability | Codex × 1 | None |
| st3 | Seat mission work | Codex × 1 | None |
| st3 | Planning Mode | Codex × 1, created by the launch API | None |
| st3 | Run Generation Revision | Codex × 1, created by the launch API | None |
| st3 | Continuous Stewardship | Codex × 1 | None |
| st3 | Agent Migration Rehearsal | Codex × 1 | None |
| st3 | Automatic GitHub Intake | Codex × 1 | None |
| st3 | Codex compaction message delivery | Codex × 1 | None |
| st3 | Native session import | Claude × 1, Codex × 1, Pi × 1, OMP × 1, OpenCode × 1 (raw, then imported) | None |

The paired and st3-only corpus has 13 Claude seats, 43 Codex seats, two Pi seats, two OMP seats,
and one OpenCode seat. It also has three Codex LLM judges.

The twenty-two model-free st3 evals add no model seats and no LLM judges.

Claude task baselines remain explicitly pinned to `claude-sonnet-5`; the native-channel wake
canary uses Claude Code's `opus` alias and records the concrete model selected by the provider.

Model agents must use a native `harness` block. A setup or fixture process can use `command`.

Codex `gpt-5.6-sol` is the default model judge.

An eval can use a Claude judge for a specific reason. Record the choice in this inventory and the run report.

## Small Talk message discipline

The graph owns planned work, assignment, dependencies, progress, products, and judgement state.

An eval does not author a harness prompt. st3 supplies the normal boot contract.

Put mission work in goals and steps. Keep only stable repository facts in `AGENTS.md`, `CLAUDE.md`, or a persona file.

Do not disable harness features or reveal held-out gate criteria in agent instructions.

st3 sends one durable Small Talk message when an assigned parent becomes ready.

Inherited nested steps use the parent message. A nested step sends a new message only when its assignee changes.

The native driver transports a graph message. It does not create a second message source.

An agent sends a direct message for a blocker, an exception, a requested result, or explicit coordination work.

An agent does not send routine progress when graph work state already expresses that progress.

An eval that tests coordination must judge the required Small Talk message sequence.

An eval must keep enough work state in the graph to survive an agent or daemon restart.

## Run reports

Each eval run gets one concise Markdown report in its eval folder.

Use `reports/YYYY-MM-DD-<run-id>.md`. Copy [run-report-template.md](./run-report-template.md) as the starting form.

Record exact values when the runtime exposes them. Mark an unavailable value and give the reason instead of estimating it.

Commit the report with the run evidence. Do not leave the only report in terminal output or temporary state.

## Validation

```sh
st2 eval ./evals/st2/license-mit
st2 eval ./evals/st2/network-smoke
cargo test -p st3 --test examples
```

st3 eval orchestration is exercised through repository integration controllers and is deliberately
not a public CLI command. The Rust test validates every st3 fixture against the normative grammar
and its structural contracts. Paid live runs must use an explicit development controller so the
public product CLI does not acquire an unfenced raw-fixture launcher.
