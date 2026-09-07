# Catalog commits and direct edits use independent wake channels

Status: accepted

Requirements change authorized by Johannes on 2026-09-03 through interview decisions Q1–Q3.

## Context

A production `axe agent new` publication waited for the next 30-second supervisor interval. Issue #430 attributed the delay to a timer-only catalog loop, but the exact reported st2 revision already installed the declaration watcher. An isolated test also showed that the watcher receives the atomic directory rename used to publish a new Agent bundle in a small catalog.

The failure is therefore at the declaration notification boundary, not the absence of a catalog event loop. Built-in st2 declaration writers already expose a stronger boundary: after locked publication and readback, each successful transaction atomically advances `.st2/catalog-generation`. Authorized direct atomic KDL replacement does not necessarily use that transaction boundary and must remain prompt.

## Options

| Option | Result | Reason |
| --- | --- | --- |
| Independent generation and declaration watchers | Selected | Gives cooperative commits a constant-cost channel while preserving direct atomic authoring. |
| Durable host-scoped reconcile request | Rejected | Adds a caller and authoring contract, host selection, and a split publish/request failure. |
| Declaration watcher only | Rejected | Leaves transactional launch latency dependent on the event health of every declaration subscription. |
| Unix socket or process signal | Rejected | Adds endpoint lifecycle, permissions, PID or socket staleness, and lossy delivery without helping direct authors. |

## Evidence and Argument

The supporting source reads, exact-revision check, caller trace, production
measurements, and watcher prototypes are recorded in
[the catalog reconcile wakeup experiment](../.experiments/2026-09-03-catalog-reconcile-wakeup.md).
The key discriminator is independence: a second watcher instance over one
control directory does not share the declaration watcher's subscription set or
backend event queue, while both preserve the existing serialized reconciliation
owner.

## Decision

The resident catalog supervisor uses two independent filesystem watcher instances that feed the same unit wake channel:

1. A constant-cost watcher subscribes non-recursively to `.st2` and accepts only mutation of `catalog-generation`. This watcher is installed before declaration subscriptions when the control directory exists, revalidates the directory identity after each pass, and reinstalls a stale subscription.
2. The declaration watcher retains shallow subscriptions to catalog declaration-space directories. It accepts `agent.kdl`, root `catalog.kdl`, `_templates`, and declaration-directory topology mutations while excluding runtime and Resource state.

A callback only queues a wake. The existing single-threaded supervisor owns reconciliation, drains a burst before one pass, and holds the shared catalog lock through discovery and execution. The periodic interval remains the correctness fallback.

Watcher setup and callback failures are operator-visible. Failure of one watcher does not remove the other watcher.

## Consequences

- `st2 agent publish`, catalog apply, and in-place st2 authoring receive a commit-aligned wake after durable publication.
- Authorized direct atomic Agent Spec replacement retains declaration-event wakeups without a publisher-specific API.
- The generation watcher has a separate backend queue, so declaration subscription volume or overflow cannot consume its events.
- The declaration watcher still has cost proportional to declaration-space directory count. This cost is accepted to preserve direct-author behavior; Resource payload depth remains excluded.
- A public kick command, Unix socket, or caller-side Axe workaround is not part of the contract.
- The exact reason that the production declaration event was missed remains unproved unless a production-shaped test reproduces it. The independent commit channel removes that unknown from transactional publication latency without mislabeling it as the root cause.

## Evidence required for acceptance

- exact-revision source evidence that the issue revision already had a catalog watcher;
- a deterministic loop test that direct publication launches an added agent before a long timer;
- atomic bundle publication coverage at the reported catalog scale;
- atomic catalog-generation replacement coverage through the independent channel;
- root `catalog.kdl`, mutation-only, disconnected-channel, and idle no-spin coverage;
- no new-record diagnostics under strict VRS validation, and the repository Nix check.
