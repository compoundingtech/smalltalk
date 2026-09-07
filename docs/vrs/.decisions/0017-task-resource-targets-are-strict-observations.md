# Task resource targets are strict observations

Status: accepted

Accepted on 2026-09-05 for R23 fail-closed task inventory.

## Context

A downstream sampler needs to attribute host CPU and memory observations to a
running st2 task every 15 seconds. st2 already has the authoritative read-only
join between desired tasks and observed PTY or exec generations, but its
version-1 inventory exposes only process metadata. Linux sampling is truthful
at the task boundary only when it uses the live process's unified cgroup-v2
membership. Systemd unit names, scope names, and task naming conventions are
not observations of that membership. Darwin has no equivalent cgroup locator;
a process-tree walk rooted at the observed PID is necessarily best effort.

The locator can disappear or change while it is read. PID alone cannot reject
reuse, and retaining scope membership in st2 would add a second state plane
whose lifecycle could diverge from the backend and kernel.

## Options

| Option | Tradeoffs |
| --- | --- |
| Add a required strict tagged target to task-inventory v2 — selected | Makes every row self-describing, represents degradation explicitly, and lets the sampler remain downstream. |
| Derive Linux targets from systemd unit names | Rejected because direct/degraded launches and cgroup migration make the name an untruthful proxy for current membership. |
| Add optional or nullable locator fields | Rejected because absence cannot distinguish non-running, indeterminate, unsupported, raced, and degraded observations. |
| Persist a runtime-ID-to-scope registry in st2 | Rejected because it creates stale ownership and reconciliation obligations without improving kernel evidence. |
| Sample and export resource metrics from st2 | Rejected because cadence, retention, and metric transport belong to the downstream observer, not the task control plane. |

## Evidence and Argument

The focused experiment separated strict cgroup parsing from the
process-generation fence. One exact unified `0::` entry preserved its
slash-prefixed path, including spaces and colons, while absent, duplicate,
relative, repeated-separator, trailing-separator, and traversal-capable shapes
were rejected. Stable start-token observations admitted the candidate target.
A deterministic PTY regression replaced the PID generation between the
registry snapshot and start-token capture; the socket-backed stats row's
changed creation generation rejected that token. A two-live-task fixture
proved one unscoped stats snapshot serves the whole inventory. A token mismatch
before target read, a token change after it, and process exit each emitted only
a bounded unavailable result and never the candidate locator.

A live Darwin exec observation joined the current PID as
`darwinProcessTree.rootPid` without rewriting its retained generation record,
and a real PTY replacement changed generation identity while retaining the
same resource-target wire contract. The complete task-inventory unit and CLI
integration surfaces preserved absence, indeterminate, park, timeout,
declaration-drift, and read-only behavior.

This evidence favors a required tagged union over nullable fields: each sample
states both whether a locator exists and why it does not. Reading kernel
membership is also strictly stronger than reconstructing it from unit naming,
while keeping sampling downstream avoids adding lifecycle and retention state
to st2.

## Decision

`st2 tasks --json` advances to `st2.task-inventory.v2`. Every runtime object has
one `resourceTarget` internally tagged by `type`:

- `{"type":"linuxCgroupV2","path":"/<exact-unified-path>"}`;
- `{"type":"darwinProcessTree","rootPid":<pid>}`; or
- `{"type":"unavailable","reason":<bounded-reason>}`.

Unavailable reasons are exactly `notRunning`, `runtimeIndeterminate`,
`processUnavailable`, `generationChanged`, `cgroupV2Unavailable`, and
`unsupportedPlatform`.

On Linux, st2 reads exactly one unified `0::<path>` entry from the observed
process's `/proc/<pid>/cgroup`. Exec compares the target fence to the token in
its generation record. PTY captures each candidate daemon token after the
registry snapshot, then takes one unscoped, socket-backed `pty stats --json`
snapshot for the entire inventory. A candidate is admitted only when its named
live stats row reports the same daemon PID and creation generation and a second
token read is unchanged; that admitted token then fences the target read. A
missing, exited, or recycled process never publishes the candidate locator.
Darwin fences the current process in the same way before exposing its PID as a
best-effort tree root. All targets are rediscovered on each inventory command.

Runtime ID remains stable task identity. PID, creation time, generation ID,
cgroup path, root PID, unit, and incarnation remain observation locators only.

## Consequences

- A downstream 15-second sampler can consume one versioned inventory without
  parsing scope names or asking st2 to sample metrics.
- Resource degradation is bounded and machine-readable while the inventory's
  existing fail-closed catalog/runtime semantics remain intact.
- The Linux path is slash-prefixed and relative to the cgroup-v2 mount; `/`
  denotes the mount root.
- st2 adds no persistent locator registry, metrics server, sampling cadence, or
  resource history.
- Version-1 strict consumers must explicitly adopt schema version 2.
- Evidence is recorded in the [task resource target experiment](../.experiments/2026-09-05-task-resource-target-observation.md).
