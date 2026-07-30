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
  validate` rather than resolving silently back to the default. Runtime
  observation has a short outer deadline so a wedged client fails the pass
  closed instead of hanging reconciliation. The deadline is containment, not
  the mechanism for admitting a larger fleet.

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
- **DQ4 Relaunch boundary (R22-R23):** Preserve R11's nondisruptive adoption
  while making launch drift visible. For each declared task, derive the desired
  launch fingerprint from a deterministic, versioned encoding of only:
  backend kind, lowered shell source or direct argv, resolved working directory,
  and the st2-managed plus declared effective environment. Tags, descriptive
  metadata, unrelated inherited environment, file contents, and other boot-time
  snapshots are not part of this fingerprint.

  st2 records the observed fingerprint only as part of its own successful task
  launch. Inspection compares desired with observed and reports:

  | State | Meaning | Healthy-task action |
  | --- | --- | --- |
  | `converged` | desired and observed fingerprints match | adopt |
  | `drifted` | both exist and differ | adopt and report drift |
  | `unknown` | no trustworthy observed fingerprint exists | adopt and report unknown |

  `unknown` includes a healthy legacy or externally adopted runtime. Catalog
  publication, supervisor restart, metadata edits, and launch-field edits do
  not implicitly disrupt any healthy task.

  Ordinary reconciliation remains sufficient after every interruption:

  | Declaration | Process | Action |
  | --- | --- | --- |
  | active | absent or dead | reap stale state and launch the latest current desired contract |
  | active | alive | adopt and report `converged`, `drifted`, or `unknown` |
  | retired | alive | stop; do not relaunch |
  | retired | absent or dead | do not launch |

  Replacing live drifted work is a separate explicit operation. Its scope is
  one selected catalog, pinned host, resolved effective PTY root, and selected
  task set. A future interface may preview drifted tasks and select one, a
  subset, or all of them; this contract does not reserve a command name. The
  operation must re-read the selected task and recheck its exact live runtime
  identity immediately before each stop. A missing, changed, wrong-host, or
  wrong-root target refuses without disruption.

  Replacement does not capture an old launch contract or boot inputs. If st2
  stops after the identity check and is then interrupted, ordinary
  absent/dead reconciliation launches the latest current desired contract.
  There is no replay of an older generation, durable operation journal,
  operation ID, phase machine, terminal receipt, or atomic old-to-new runtime
  transition. A task rename is the explicit sequence retire old, then add new.

  This entire lifecycle works from an ordinary copied or synchronized catalog
  folder. CAS may later add publication, history, or storage optimization, but
  fingerprinting, inspection, reconciliation, replacement, retirement,
  recovery, and rename must neither require nor become incomplete without it.

  Executable acceptance proves:

  1. metadata, tags, and Resource-only edits preserve the fingerprint and live
     runtime, while kind, launch, resolved-cwd, or effective-environment edits
     report `drifted` without changing runtime identity;
  2. a healthy legacy runtime reports `unknown` and remains unchanged;
  3. natural exit or death launches once from the latest current declaration
     and records that launch's observed fingerprint;
  4. explicit replacement refuses stale identity or scope, and affects only
     the selected drifted tasks;
  5. interruption after stop heals through ordinary reconciliation to the
     latest current desired contract, without old-state replay;
  6. retirement stops and prevents relaunch, while rename works as
     retire-old/add-new; and
  7. the same proofs pass using only a plain local catalog folder with no CAS
     service, CAS metadata, database, or network dependency.

  The executable acceptance above resolves this open implementation design.
  See [#40](https://github.com/compoundingtech/st2/issues/40),
  [#41](https://github.com/compoundingtech/st2/issues/41),
  [#44](https://github.com/compoundingtech/st2/issues/44), and
  [#60](https://github.com/compoundingtech/st2/issues/60).
