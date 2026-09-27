# st2 owns the direct actor lifecycle

Status: accepted

Accepted on 2026-09-25 for R47 published actor resource roots and R48 direct
actor archival.

## Context

A direct OMP session runs without a declaration. Its entrypoint derives
`ST_AGENT=<host>.direct.omp.<encoded-pty-id>` from `PTY_SESSION`, and writers
lazily create `agents/<host>/direct.omp.<encoded-pty-id>/` in the live catalog.
Nothing ever removed those directories: they have no declaration, so they can
never be retired, and the retired-seat archive step never saw them. Every
direct session ever started therefore stayed in the live catalog, and catalog
viewers had to guess where one actor ended and the next began.

Three facts bear on who should remove them. st2 already owns the structural
archive (`st2 catalog archive`/`unarchive`, tombstones, the `archive-after`
grace period, the 25-per-pass bound, and the exclusive authoring lock). The
supervisor pass already lists the catalog's effective PTY registry. The
direct entrypoint, by contrast, runs only while its session is alive, so it
cannot observe its own death.

Liveness has several tempting proxies: directory mtimes, the newest file under
`resources/`, harness activity records, or matching PTY names against the
identity. Each can be wrong in both directions. A long-idle live session has
old mtimes. A synced or restored directory has fresh ones. PTY names are not
unique, and a session ID can be a prefix or substring of another.

## Options

| Option | Tradeoffs |
| --- | --- |
| st2's supervisor archives dead direct actors through the existing archive — selected | Reuses one reversible mechanism, lock, grace period, and bound; the registry read is already in the pass; no new owner or state plane. |
| The direct entrypoint cleans up its own directory on exit | Rejected because a crashed, killed, or host-rebooted session never runs its exit path, and deleting on exit is irreversible. |
| A separate sweeper outside st2 | Rejected because it would duplicate the archive, lock, ledger, and registry read, and race `st2 catalog apply` without st2's authoring lock. |
| Liveness from mtimes, activity, or name matching | Rejected because each proxy misjudges idle-live and restored-dead actors, and a wrong verdict moves a live actor's state out from under it. |
| Delete dead directories instead of archiving | Rejected because deletion is irreversible; retention is a separate question. |

## Evidence and Argument

The strict PTY segment codec makes the directory name alone yield the exact
PTY session ID: a PTY-generated eight-character ID is its own segment, every
other valid ID is `x-` plus its lowercase hex bytes, and only the canonical
encoding decodes. That gives one exact join key into the registry, so the
liveness verdict is a lookup, not an inference. A unit test pins that a
running session whose ID merely contains an actor's ID does not keep that
actor alive, and that an exited record is the same as an absent one.

A first-observation ledger is needed because st2 records no timestamp when a
session dies, exactly as for retirement. Dropping a row when the PTY runs
again, and when the actor is selected for archival, errs toward keeping
actors live: a restart, a failed move, and an unarchive each start a fresh
grace period. The supervisor integration suite
(`tests/supervisor_auto_archive.rs`) covers grace expiry for exited and absent
records, a stale row dropped for a running PTY, a first-observed death
serving its own grace period, a byte-identical whole-directory move with the
decoded PTY ID in the tombstone reason, and unarchive without immediate
re-archival.

Publishing each actor's resource root in the graph removes the viewer's need
to infer subject boundaries, and the same discovery feeds both publication and
archival, so the two cannot disagree about what a direct actor is.

## Decision

st2 owns the direct actor lifecycle. `st2 catalog graph --json` publishes
`resourceRoot` for every agent and a `directActors` list, additively within
`st2.catalog-graph.v2`. The supervisor's archive step archives local direct
actors whose PTY session has had no running record in the catalog's effective
PTY registry for `archive-after`, measured from first observed death in
`.st2/direct-dead-observed.json`, after retired seats and within the same
per-pass bound, as a reversible rename plus tombstone. Liveness uses only the
decoded PTY ID and exact registry records; mtimes, activity, and names never
decide it.

## Consequences

- The direct entrypoint needs no exit hook, and a crashed session is archived
  like a cleanly exited one.
- A direct actor on another host is never judged, because that host's
  registry is not observable; its own supervisor archives it.
- A PTY registry that loses a live session's record makes that actor look
  dead; the grace period and reversible unarchive bound the cost.
- `st2 catalog archive` stays declaration-only; the supervisor step is the
  only path by which a direct actor leaves.
- Archival moves bytes without reclaiming them. Retention of archived
  identities is open as spec DQ6
  ([#530](https://github.com/compoundingtech/st2/issues/530)).
