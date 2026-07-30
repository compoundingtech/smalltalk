# st2 specification

This document specifies st2's current implementation contract. It builds on
[requirements.md](./requirements.md).

## Status

Active. This is a concise map to the implementation and its evidence, not a
replacement for the README, CLI help, KDL examples, or tests.

## Scope

st2 validates a declared agent fleet, materializes agent workspaces, launches
host-local work, adopts and supervises independently surviving tasks, and
delivers messages. The agent grammar and harness-facing contract remain
canonical in
[`compoundingtech/evals/AGENT-SPEC.md`](https://github.com/compoundingtech/evals/blob/main/AGENT-SPEC.md).

## Host-local scheduling and supervision

```text
hand-authored KDL
       │
       ▼
validate ──► materialize ──► host-local st2 scheduler/reconciler
                                      │
                             ┌────────┴────────┐
                             ▼                 ▼
                        PTY / exec       DING sidecar
                             │                 │
                             └──── state + bus ┘
                                      ▲
                                      │
                            one intelligent root agent
                         observes · recovers · escalates
```

- **R01–R03:** Fleet validation separates structural errors from selected-host
  runtime facts. Materialization is inspectable and host reconciliation starts
  only declarations pinned to the local host.
- **R04:** Each machine schedules and reconciles only its pinned work. The st2
  loop is deterministic; exactly one declared root agent provides intelligent
  host-local supervision, bounded recovery, and escalation. Filesystem reads
  never wake reconciliation; only create, modify, rename, or remove events may
  wake it before the bounded timer.
- **R06:** st2 passes the complete effective task definition to the underlying
  launcher so manual and supervised restarts are equivalent. Harness readiness
  that depends on a dynamically selected account belongs to that declared
  command. In particular, reconciliation never mutates an ambient Codex config
  before launch: the command may select an account-specific `CODEX_HOME` only
  after st2 starts it. `st2 pretrust` remains an explicit operator utility for
  commands that intentionally use the ambient Claude and Codex configs.
- **R07:** Hook bundles are explicit, content-addressed, installed separately,
  and verified before materialization references them. Their receipts use the
  same resolved build identity as the binary's version surfaces for both
  hermetic package builds and source builds. Installation automatically accepts
  an ordered upgrade; `--replace` is the explicit exact-state authority for a
  downgrade, an unorderable build, or an unreadable receipt. Shipped hooks
  resolve Bash through `PATH`; the Nix package executes their integration gate
  with Bash and `jq` declared. Runtime materialization verifies the invoking
  binary's own content-addressed set, independent of which installed binary the
  receipt currently selects, so old and new supervisors can overlap during
  cutover. `hooks verify-own` exposes that read-only capability to package
  activation tooling.
- **R11:** `st2 up` is a replaceable control plane, not the lifetime owner of
  its agents. Normal exit, forced termination, binary replacement, and restart
  leave every running agent PID and creation identity unchanged. The new
  control plane adopts those processes and starts only missing work; it does
  not duplicate them. Agent stop or retirement requires a separate explicit
  lifecycle action.

  Executable acceptance starts an agent, terminates `st2 up` normally and with
  a forced kill, verifies the agent remains alive and usable, replaces the st2
  binary, starts the control plane again, and proves adoption with the same
  agent PID/creation identity and no duplicate process.

- **Session registry:** A catalog owns the `pty` registry holding its tasks.
  `<catalog>/pty` is the default; a catalog may declare another so that one host
  can share a single registry across catalogs. Resolution is an exported
  `PTY_ROOT`, then the catalog's declaration, then the default, applied
  uniformly to spawn, list, kill, and the bus environment st2 hands to native
  tools, so every reader that can resolve the catalog agrees about where its
  sessions are. A declaration whose field set does not match fails `st2
  validate` rather than resolving silently back to the default.

## Message lifecycle

```text
atomic inbox file → DING attempt → agent reads → archive receipt
       └──────── archive with same filename wins ────────┘
```

- **R05:** A matching archive filename makes an inbox copy handled; stale
  duplicates are removed without another DING. Fresh `dnd` suppresses delivery;
  `busy` does not. Failed delivery remains retryable. Sidecar restart emits a
  bounded recovery notice instead of replaying the inbox. Delivery may wake an
  agent while it is working, but an active or uncertain human composer must be
  left untouched. Unsafe delivery retries use a bounded backoff so an active
  composer cannot create a short-lived PTY probe on every inbox poll. Inbox
  reads do not wake the sidecar; only mutations bypass its bounded poll cadence.

## State and scope

- **R08:** Presence and activity status are separate signals. The catalog must
  also expose the agent's current plan and step with explicit freshness so a
  human or supervising agent can understand progress without PTY inspection.
  Current presence/status files provide only part of this contract; the
  canonical plan-progress shape is not yet specified.
- **R09:** Durable work state is external to the model transcript and is
  restored into replacement sessions through declared workspace files and
  verified hooks.
- **R10:** Fleet identities are agents. General-purpose identity kinds are
  unsupported.

The owner updates this spec whenever implementation changes.
Changing [vision.md](./vision.md) or [requirements.md](./requirements.md)
requires Nathan's explicit approval.

## Event contracts (R13–R15)

An event is evidence, not permission to run the world. The reconciler retains
path and kind, maps them to the affected identity or template dependency set,
and computes the smallest desired-versus-actual delta. One declaration affects
only that agent; a template affects only its dependents. A no-op performs zero
PTY queries, launches, teardowns, materialization, or writes.

Watchers are deny-by-default. The classifier/action contract is:

| Event | Minimal action |
| --- | --- |
| `agents/**/agent.kdl` create/modify/remove | validate, materialize, and converge that agent and derived tasks |
| referenced `_templates/**` mutation | converge dependent agents only |
| inbox create/archive/remove | DING consumer only; supervisor no-op |
| plan/resource/status mutation | specialized consumer only; supervisor no-op |
| PTY/exec/log/PID/socket/lock/temp/backup/read/open/unknown | no-op |

Startup, timer, watcher overflow/loss, and ambiguity are bounded full-audit
fallbacks. Accepted streams use head/tail coalescing: immediate head response,
one quiet tail after a burst, and a hard maximum. Executable proof covers
positive declaration/template wakes, negative runtime/bus events, bounded
discovery/materialization/PTY queries and writes, continuous-event starvation,
and no-op desired-equals-actual behavior.

## Targeted reconciliation (R19)

`st2 up --materialize-only --task <host.agent.task>` resolves one exact local
task before writing and renders only its owning agent. `st2 up --once --task
<host.agent.task>` performs the same owner-only materialization, then inspects
PTY/exec state and executes a plan containing only that task. Unknown,
ambiguous, and wrong-host selectors refuse before writes or runner inspection;
unrelated discovery diagnostics remain visible without preventing the selected
owner/task path.

`st2 up --materialize-only --agent <id>` remains the agent-wide rendering
selector. Targeted task reconciliation is intentionally bounded to `--once`;
the resident supervisor continues to reconcile the complete local catalog.

## Open design questions

- **DQ1 Scheduled work:** The vision includes per-machine schedulers that form a
  distributed workflow engine, but the KDL shape, event inbox, deduplication
  boundary, and execution receipts are not yet specified. A successful
  executable eval and Nathan's approval should resolve this before adding
  scheduler requirements.
- **DQ2 Safe DING delivery:** Bounded observation now replaces the fixed
  paste-to-Return delay: maintained Codex and Claude composers must be
  positively empty before paste and show the exact staged notice twice before
  a separate Return. Human, modal, active, changed, timed-out, and unknown
  states fail closed, with staged-payload ownership preventing duplicate paste.
  This measured screen heuristic is still not an evented proof and renderer
  changes may defer delivery. Resolve the remaining gap with a stronger evented
  signal or other measured classifier; a small on-device model is an optional
  experiment, not a required architecture.
- **DQ3 Catalog agent state:** Define the catalog paths, schemas, freshness
  rules, and atomic update semantics for presence, activity status, current
  plan, and current plan step. Prove that stale state is distinguishable and
  that a supervisor can follow plan progress without inspecting a PTY before
  adding the shape to `AGENT-SPEC.md`.
- **DQ4 Relaunch boundary:** Ratify and implement a generation-aware lifecycle
  that preserves R11's nondisruptive adoption while making launch-definition
  drift explicit. The model has three separate identities:

  | Identity | Meaning | Changes when |
  | --- | --- | --- |
  | declaration revision | the accepted canonical Agent Spec declaration | any declaration content changes |
  | launch generation | a versioned digest of one lowered effective spawn contract | a value needed to reproduce that task's launch changes |
  | replacement intent | explicit task-scoped authority to disrupt a live process | an operator requests a compare-and-replace transition |

  Assignment, metadata, and Resource binding changes advance the declaration
  revision but do not change a task's launch generation unless they lower into
  its effective spawn contract. They never imply replacement. The launch
  generation covers stable task identity, backend/kind, command or argv,
  working directory, complete effective environment, and every materialized
  input consumed at process start. It uses a deterministic, versioned canonical
  encoding; st2 persists the observed value beside runtime identity when it
  launches a task.

  Inspection exposes declaration revision plus desired and observed launch
  generations. Their relation is a tagged state: `converged`, `drifted`, or
  `unknown`. `unknown` covers a surviving legacy process without recorded
  launch identity and remains nondisruptive.

  Reconciliation follows this matrix:

  | Declaration | Process | Generation relation | Intent | Action |
  | --- | --- | --- | --- | --- |
  | active | absent or dead | any | none | reap stale state and launch the latest desired generation |
  | active | alive | converged | none | adopt |
  | active | alive | drifted | none | adopt and report drift |
  | active | alive | unknown | none | adopt and report unknown |
  | active | alive | converged, drifted, or unknown | pending replacement matching the exact runtime identity | durably record the stop phase, replace, and persist the terminal receipt |
  | active | absent or dead | any | replacement whose stop phase names the prior runtime | resume the operation, launch the requested desired generation once, and persist the terminal receipt |
  | active | alive | any | stale or mismatched replacement | reject without disruption |
  | retired | alive | any | retirement | tear down and do not relaunch |
  | retired | absent or dead | any | retirement | retain retired state; do not launch |

  Publication and reconciliation are observational for a healthy process.
  Automatic relaunch occurs only when an active declared task has no live
  process. Intentional replacement of a live task is a separate task-scoped
  one-shot operation with a stable operation identity. It must name the exact
  expected runtime identity and desired generation; the observed generation is
  additional drift evidence, never a substitute for runtime identity. Before
  stopping the process, st2 durably advances the operation from `pending` to a
  stop phase naming that runtime. Reconciliation resumes an incomplete
  operation from its durable phase rather than matching it against a successor
  that happens to have the same launch generation. A terminal replacement
  receipt is idempotently keyed by operation identity and records the prior
  runtime, requested transition, stop result, launch result, and resulting
  runtime, so a crash after stop or launch neither strands the task nor replaces
  its successor again.

  Changing task identity is not replacement. If the old identity is live, st2
  must not silently orphan it or launch a duplicate successor. The transition
  is rejected until explicit retirement, or one atomic old-to-new replacement
  intent authorizes and receipts both sides. Control-plane replacement never
  supplies that authority.

  Executable acceptance proves:

  1. assignment, metadata, and Resource-only edits preserve PID, creation
     identity, and observed launch generation;
  2. command, argv, cwd, effective-environment, and boot-input edits preserve a
     healthy PID while exposing `drifted`;
  3. normal and forced `st2 up` replacement preserves the live runtime and its
     observed generation;
  4. natural exit or death launches exactly one process at the latest desired
     generation;
  5. matching replacement changes runtime identity exactly once, including
     same-generation replacement and replay after a crash before terminal
     receipt persistence, while a stale request changes nothing;
  6. retirement tears down and subsequent reconciliation does not relaunch;
  7. identity transitions cannot leave an unreported orphan or duplicate;
  8. legacy runtime records are exposed as `unknown` and adopted without
     disruption; and
  9. a crash or launch failure after replacement stops the prior runtime resumes
     the same operation and terminates with one receipt rather than stranding
     the task or replaying replacement against its successor.

  This requires Nathan's approval of the corresponding protected requirements
  before it moves from design question to implementation contract. See
  [#40](https://github.com/compoundingtech/st2/issues/40),
  [#41](https://github.com/compoundingtech/st2/issues/41),
  [#44](https://github.com/compoundingtech/st2/issues/44), and
  [#60](https://github.com/compoundingtech/st2/issues/60).
