# Clean operational state recovery audit

Date: 2026-09-20

This is the immutable-evidence handoff for failed run
`mission-run/fleet/st3/clean-operational-state/01a0bf17928a7b70976f5b30f78c8e9c`.
It records what can be reused, what remains unfinished, and three deterministic red baselines. It
does not reinterpret the failed run as successful and does not change runtime behavior.

Resolution update, 2026-09-21: the three recovery blockers below are retained as historical
incident evidence, but they are no longer pending release baselines. Claimed-interval timeout and
nested-terminal cleanup landed in `948a161`; durable native-driver wake, retry, acknowledgement,
diagnostics, and manual wake landed in `85cea78`. Their executable regressions are
`only_claimed_execution_consumes_a_step_timeout_and_failure_selects_cleanup`,
`terminal_roots_reap_nested_orphans_idempotently_across_restart_and_replication`,
`work_wakes_retry_with_a_bound_and_stop_after_acknowledgement`, and
`manual_work_wake_is_idempotent_and_uses_the_durable_driver_inbox`. The operational-state contract
suite now treats the fixture `observed` fields strictly as immutable before-fix evidence and has no
ignored red release test.

## Audit scope and result

The failed run used mission revision
`2266cdf7325e423665abd69534d250051dcbaadb217ad4b54804a8b69df48531` and generation
`run-generation/01a0bf17934a7a03b81bd4931c507f1f`. Its final graph state is `failed / terminal`:
7 of 20 steps completed, `clean-cli-defaults` failed, and the other 12 ordinary steps were
cancelled with `another step failed`. The terminal run and all cancellation evidence remain
immutable.

The required implementation worktree was clean at inspection, on
`agent/st3-clean-operational-state`, with `fb94602` at `HEAD`. The source worktree
The st3 working tree was not changed. The four required commits
exist in order and are reused, not rewritten:

```
f28da55 docs(st3): define client v0 contract
9c2e7a2 docs(st3): define operational state layers
5b6e78c fix(st3): fence terminal work ownership
fb94602 Expose truthful operational projections
```

Disposition vocabulary:

- **preserved**: accepted evidence or implementation remains correct and is reused as-is;
- **incomplete**: useful work exists, but the original goal is not yet fully satisfied;
- **invalidated**: later deterministic evidence contradicts the completion claim at its stated
  breadth; correct narrower implementation is still preserved;
- **new work**: a release obligation discovered after the original mission was published.

## Before operational tree

This is the concise graph state before recovery behavior changes:

```
clean-operational-state run 01a0bf…c8e9c  [failed, terminal]
├─ 7 completed
│  ├─ materialize                         [preserved worktree]
│  ├─ contract-and-fixtures               [9c2e7a2]
│  ├─ client-v0-contract-and-fixtures     [f28da55]
│  ├─ contract-review                     [human gate completed]
│  ├─ terminal-and-lease-lifecycle        [5b6e78c; breadth invalidated below]
│  ├─ truthful-projections-and-api        [fb94602]
│  └─ stop-mission-agents                 [cleanup completed]
├─ clean-cli-defaults                     [failed: timeout, never claimed]
└─ 12 ordinary descendants                [cancelled: another step failed]

cross-run-coordination-v0 run 01a0c01f…45f5c [failed, terminal]
├─ bootstrap-after-clean-state            [failed: prerequisite success is unsatisfiable]
├─ 12 implementation/review/landing steps [cancelled]
└─ stop-mission-agents                     [completed]

switcher-native-search root run 2026-09-20 [failed, terminal]
└─ nested run 3c9a…7bdde                   [incorrectly still running]
   └─ use-the-native-field 04b2…/…         [incorrectly ready and offered]

standing app-web agent                     [alive]
├─ active work                             [unrelated live step]
└─ orphan use-the-native-field             [incorrect extra ready step]
```

The cross-run coordinator is therefore not silently waiting. Its bootstrap step recorded the
exact failed prerequisite, the absent `land-and-verify` completion, and the recovery planning
identity, then failed. Recovery must publish a supported successor or handoff after a real landing;
the old coordinator must remain failed.

## Seven completed artifacts

| Completed step or artifact | Disposition | Recovery use |
|---|---|---|
| `materialize` | **preserved** | Reuse the registered clean worktree and required branch. |
| `client-v0-contract-and-fixtures` / `f28da55` | **preserved** | Reuse the schema, route/action manifest, fixtures, docs, and conformance baselines. |
| `contract-and-fixtures` / `9c2e7a2` | **preserved** | Reuse the three-layer contract, command inventory, fixtures, and red baselines. |
| `contract-review` | **preserved** | The human gate accepted those contracts. This is contract acceptance, not implementation acceptance. |
| `terminal-and-lease-lifecycle` / `5b6e78c` | **preserved** | Keep its direct generation/run terminalization, lease fencing, stale-claim rejection, restart, and replay work. Extend it for nested descendants and terminal cancellation. |
| Unqualified `5b6e78c` completion statement that every descendant is retired | **invalidated** | `terminal-nested-orphan.json` proves a terminal root can leave its nested run running and its step ready. |
| `truthful-projections-and-api` / `fb94602` | **preserved** | Reuse paginated client read projections, annotations, and default/history filtering. Broader CLI and action work remains. |
| `stop-mission-agents` | **preserved** | Both failed-run agents were stopped; this is cleanup evidence only. |

The two graph-only completions (`contract-review` and `stop-mission-agents`) intentionally have no
commit. No cancelled step is treated as completed.

## Goal reuse matrix

### Mission-level goals

| Goal | Disposition | Evidence and successor |
|---|---|---|
| M1: continuously truthful current state, APIs, and default CLI views while preserving history | **incomplete** | `5b6e78c` and `fb94602` are the base. Finish in `timeout-and-terminal-lifecycle` and `operational-cli-attention`, then audit in `conformance-and-cli-audit`. |
| M2: graph-authorized repair for terminal owners, expired leases, stopped agents, stale attention, and eval noise | **incomplete** | No repair step completed. Finish in `repair-and-doctor` and prove live effect in `live-proof-and-repair`. |
| M3: complete client v0, launch workflow, normalized conversation, visualization, synchronization, generated clients, actions, Fabric access, and terminal control | **incomplete** | The contract is preserved in `f28da55`; implementation moves to `launch-and-plan-visualization`, `client-v0-contract`, and `conformance-and-cli-audit`. |

### Step goals

Every goal-bearing step from the original revision appears below. `G1`, `G2`, and `G3` retain the
original order within that step.

| Original goal | Disposition | Preserved base and remaining owner |
|---|---|---|
| `materialize.G1` — create or reuse the isolated clean worktree and branch | **preserved** | Worktree and branch were clean at `fb94602`; continue using them until reviewed landing. |
| `contract-and-fixtures.G1` — define history, projection, and actionable layers including ownership, leases, identity, traffic, and no compatibility mode | **preserved** | `9c2e7a2`; new findings extend rather than replace the contract. |
| `contract-and-fixtures.G2` — deterministic fixtures for terminal, lease, stopped, superseded, reminder, and eval cases | **preserved** | `9c2e7a2`; this audit adds three separate recovery fixtures. |
| `contract-and-fixtures.G3` — red assertions before behavior changes | **preserved** | Original ignored baselines remain; the recovery blockers are also captured before behavior changes. |
| `client-v0-contract-and-fixtures.G1` — list/detail/action and session-timeline contracts | **preserved** | `f28da55`; implement in `client-v0-contract`. |
| `client-v0-contract-and-fixtures.G2` — snapshot, cursor, event, error, action, preview, schema, and fixture contracts | **preserved** | `f28da55`; complete event/action conformance in `client-v0-contract` and `conformance-and-cli-audit`. |
| `client-v0-contract-and-fixtures.G3` — Unix and paired Fabric thin-client/terminal contract | **preserved** | `f28da55`; transport and authorization implementation remains in `client-v0-contract`. |
| `terminal-and-lease-lifecycle.G1` — terminal owners retire every state and clear leases | **invalidated** | `5b6e78c` correctly handles direct steps, but a terminal root left nested run `3c9a…7bdde` and step `04b2…` live. Extend in `timeout-and-terminal-lifecycle`. |
| `terminal-and-lease-lifecycle.G2` — owner/lease checks make default work truthful even after missed reconciliation | **invalidated** | The orphan remained visible in `work ls`; default selection must reject terminal ancestry and missing/contradictory ownership. |
| `terminal-and-lease-lifecycle.G3` — restart and replication replay converge | **incomplete** | The preserved direct-owner proof passes. Add nested-root terminal, cancellation, stale claim, restart, and replication cases in `timeout-and-terminal-lifecycle`. |
| `truthful-projections-and-api.G1` — stable paginated current/history shapes and renderer membership parity | **incomplete** | `fb94602` supplies the versioned paginated read slice. Complete all human/JSON/CLI parity in `operational-cli-attention` and `conformance-and-cli-audit`. |
| `truthful-projections-and-api.G2` — default agents include desired unhealthy agents and exclude stopped/superseded/terminal/eval history | **preserved** | `fb94602`; focused stopped-agent history/default test passes. |
| `truthful-projections-and-api.G3` — compatibility-free operational defaults with annotated history | **preserved** | `fb94602` implements the covered work/agent projections; command-wide audit remains a separate incomplete goal. |
| `clean-cli-defaults.G1` — operational work/agent defaults and explicit person authority | **incomplete** | The step never started. Recovered by `operational-cli-attention` and audited by `conformance-and-cli-audit`. |
| `clean-cli-defaults.G2` — compact lists plus detail/show paths | **incomplete** | Same successor; no completion artifact exists. |
| `clean-cli-defaults.G3` — formatted counts equal JSON membership | **incomplete** | Same successor; include every CLI and JSON form. |
| `launch-workflow-and-visualization.G1` — replace public planning with planner-backed launch lifecycle | **incomplete** | No implementation artifact. Assigned to `launch-and-plan-visualization`. |
| `launch-workflow-and-visualization.G2` — normalized native planner conversation | **incomplete** | Contract portions in `f28da55` are reusable; implementation remains in `launch-and-plan-visualization`. |
| `launch-workflow-and-visualization.G3` — complete typed plan/live visualization | **incomplete** | Contract portions are reusable; implementation remains in `launch-and-plan-visualization`. |
| `attention-and-eval-lifecycle.G1` — supersede stale failure/readiness/reminder attention | **incomplete** | Some projection helpers landed in `fb94602`; full lifecycle belongs to `operational-cli-attention`. |
| `attention-and-eval-lifecycle.G2` — one typed attention family with kind-specific lifecycle | **incomplete** | Finish in `operational-cli-attention`. |
| `attention-and-eval-lifecycle.G3` — explicit operational/eval/test traffic classes and isolation | **incomplete** | Contract is preserved in `9c2e7a2`; implementation remains in `operational-cli-attention`. |
| `client-read-models-and-spec.G1` — stable bounded list/detail/action models for all v0 resources | **incomplete** | `fb94602` implements a real read slice; complete resource/action coverage in `client-v0-contract`. |
| `client-read-models-and-spec.G2` — harness-neutral ordered conversation timeline | **incomplete** | Schema/fixture contract in `f28da55`; implementation remains in `client-v0-contract`. |
| `client-read-models-and-spec.G3` — machine spec, fixtures, reusable Rust client, generated Swift client, deterministic regeneration | **incomplete** | Spec/fixtures are preserved; both reusable/generated clients and byte-stable generation remain in `client-v0-contract`. |
| `client-sync-actions-and-fabric.G1` — atomic snapshots, long-poll events, reconnect, gap/full-resync, disconnected read-only cache | **incomplete** | Snapshot pagination base exists in `fb94602`; complete synchronization in `client-v0-contract`. |
| `client-sync-actions-and-fabric.G2` — fenced idempotent actions and session-derived remote actors | **incomplete** | Action contract exists in `f28da55`; implementation remains in `client-v0-contract`. |
| `client-sync-actions-and-fabric.G3` — authenticated Fabric-carried loopback gateway and fenced terminal stream | **incomplete** | Transport contract exists in `f28da55`; implementation remains in `client-v0-contract`. |
| `client-contract-proof.G1` — exercise every v0 flow through the reusable client | **incomplete** | No proof artifact. Assigned to `conformance-and-cli-audit`. |
| `client-contract-proof.G2` — identical Unix/Fabric-like flow including reconnect, gaps, fencing, revocation, and escalation rejection | **incomplete** | Assigned to `conformance-and-cli-audit`. |
| `client-contract-proof.G3` — byte-stable Rust/Swift regeneration and fixture validation | **incomplete** | Assigned across `client-v0-contract` and `conformance-and-cli-audit`. |
| `repair-and-diagnostics.G1` — graph-authorized repair dry-run/apply | **incomplete** | No artifact; implement in `repair-and-doctor`. |
| `repair-and-diagnostics.G2` — read-only doctor with projection-health guidance | **incomplete** | No artifact; implement in `repair-and-doctor`. |
| `repair-and-diagnostics.G3` — restart/retry/replication-safe copied-state proof | **incomplete** | No artifact; prove in `repair-and-doctor`. |
| `independent-review.G1` — read-only held-out review of lifecycle, repair, launch, client, auth, and isolation | **incomplete** | Original review was cancelled; perform after all implementation in `independent-review`. |
| `independent-review.G2` — enumerate correctness, loss, hidden-fault, escalation, cursor, fencing, protocol, and compatibility risks | **incomplete** | Same successor; no prior review report exists. |
| `verification.G1` — resolve all material review findings | **incomplete** | Assigned to `resolve-and-verify`. |
| `verification.G2` — focused and full test/eval/restart/replay/repair proof | **incomplete** | Assigned to `resolve-and-verify`. |
| `verification.G3` — record exact commands, counts, revisions, fixtures, and membership | **incomplete** | Assigned to `resolve-and-verify`. |
| `repair-live-graph.G1` — apply only reviewed repair and prove second apply is zero-change | **incomplete** | Assigned to `live-proof-and-repair`; no live apply is authorized by this audit. |
| `repair-live-graph.G2` — verify ghosts/noise leave defaults while history and live faults remain | **incomplete** | Assigned to `live-proof-and-repair`. |
| `land-and-verify.G1` — integrate only approved revision, rebuild/install, retain clean worktrees | **incomplete** | Assigned to `land-install-and-smoke`; this audit does not land. |
| `land-and-verify.G2` — installed projection/client/regression/restart/replication proof | **incomplete** | Assigned to `land-install-and-smoke`. |
| `land-and-verify.G3` — record landed revision and exact operational counts | **incomplete** | Assigned to `land-install-and-smoke` and final `coordination-handoff`. |

The original `contract-review`, `live-repair-review`, `landing-review`, and
`stop-mission-agents` steps have no step goals. Their gate/cleanup disposition is represented in
the completed-artifact table and cancellation evidence rather than inventing goals for them.

## New work discovered by recovery

| New obligation | Disposition | Recovery owner |
|---|---|---|
| A ready, never-claimed step must not consume its active execution budget | **new work** | `timeout-and-terminal-lifecycle` |
| Terminalization must recursively cancel nested mission runs and steps; terminal cancellation must reap or report an explicit no-op | **new work** | `timeout-and-terminal-lifecycle` |
| Work output must show owner run identity and terminal/non-actionable reason, including duplicate step names | **new work** | `timeout-and-terminal-lifecycle` and `operational-cli-attention` |
| Ready assigned work must wake Codex, Claude, and OMP through a supported driver path with acknowledgement, bounded retry, and diagnostics | **new work** | `wake-contract` |
| Manual wake must use the same driver path and never synthesize terminal text or Enter | **new work** | `wake-contract` |
| The failed cross-run coordinator needs a supported successor/replacement handoff after a real landing | **new work** | `coordination-handoff` |

## Deterministic reproductions and root causes

### Ready-step timeout

Fixture: `ready-step-timeout.json`.

`clean-cli-defaults` became ready at `1789920739909` and failed at `1789931540058`, an elapsed
`10,800,149ms` against its `10,800,000ms` timeout. There is no `work.claimed`, `work.progress`, or
execution-start event between them.

`Reconciler::step_timed_out` reads the latest `step-run.state` claim and subtracts that claim's
acceptance time from wall clock for every `ready`, `claimed`, `working`, `verifying`, or `blocked`
view. The ready claim therefore starts the execution clock. Dependency wait, assignment wait, and
unclaimed ready time are indistinguishable from active execution time. The required clock starts
only from a durable active claim/execution interval and must expose its basis in detail output.

### Terminal nested orphan

Fixture: `terminal-nested-orphan.json`; source message: `message/b064cecaaf0b3597`.

The root run is `failed / terminal`, while nested run
`mission-run/3c9a75147bc05e883a4f8a5b4f37bdde` remains `running` and its only step remains `ready`.
`work ls --as agent/fleet/app-web/standing/app-web` still offers that step next to unrelated live
work. A cancellation accepted against the terminal root did not reap it.

The generic terminal transition in `store.rs` terminalizes steps owned directly by the selected run
generation. Descendant-run cancellation exists for selected timeout and revision paths, but is not
an invariant of every terminal transition. The terminal cancellation path also returns before
repairing descendants. Query selection can consequently surface a ready child whose root ancestry
is terminal, while status projection does not expose its canonical owner run. Fix all three layers:
the transition, the query fence, and the idempotent terminal cancellation/repair response.

### Ready idle harness wake

Fixture: `ready-idle-wake.json`; accepted report:
`doc/cos/st3-idle-with-ready-work-2026-09-20@4ea5b236c3b5fb8cabee06dba238464c9c8270afcc0211b87407b0d494f238ac`.

Twice, the exact assignee was alive and idle with zero active claims and one ready step. Four forms
of Enter/return input were accepted without starting a turn. Only injecting the complete boot
prompt started a turn. In a related incident, an at sign in injected wake text opened a file picker
and consumed all loop rounds.

`reconcile_work_messages` emits a Small Talk message. `run_ding_driver` then writes a DING string as
line input through the session/terminal surface. There is no driver-level wake contract, no
turn-or-claim acknowledgement, no bounded retry, no exhausted-wake `harness.diagnostic`, and no
manual command that invokes the same supported path. Terminal composer state is therefore part of
delivery correctness. Recovery must replace terminal-text wakeup rather than add more key or prompt
variants.

Resolution: automatic and manual work wake now create incarnation-bound durable messages consumed
by the maintained native drivers. A working-turn observation or work claim acknowledges delivery;
the reconciler schedules bounded retries and publishes `work-wake-exhausted` after the third
unacknowledged attempt. The legacy st3 DING driver and terminal-line injection path were removed.

## Preserved validation

The following focused commands passed on `fb94602` before relying on the preserved commits:

```
cargo test -p st3 --test client_v0_contract
# 10 passed; 1 ignored red action baseline

cargo test -p st3 --test operational_state_contract
# 9 passed; 1 ignored CLI baseline (before adding the recovery fixtures)

cargo test -p st3 --lib terminal_owners_retire_every_work_state_across_restart_and_replication
# 1 passed

cargo test -p st3 --test client_v0_contract operational_lists_share_one_versioned_paginated_shape
# 1 passed

cargo test -p st3 --test client_v0_contract stopped_agents_are_annotated_history_not_default_membership
# 1 passed
```

The recovery fixtures are intentionally small JSON inputs. Their self-consistency test preserves
the historical observation, while the green executable regressions named in the resolution update
prove the required runtime behavior.

## Cancellation and handoff invariants

- The old run remains `failed / terminal`; no recovery claim forges or imports success.
- Its 12 `another step failed` cancellations remain valid evidence of work not performed.
- The coordinator run remains failed because its explicit prerequisite success predicate was
  unsatisfiable. The recovery run must publish a successor or handoff, not mutate that verdict.
- `f28da55`, `9c2e7a2`, `5b6e78c`, and `fb94602` stay in history and on the branch. Correct code is
  extended; it is not destructively reset or reimplemented.
- No live repair or landing is authorized by this audit. Those actions retain their own review,
  verification, installed-build, and idempotency obligations.
