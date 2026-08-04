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

## Canonical Agent Spec eval teams

An eval may opt into `canonical-agents` after its fixture copy and deterministic
run steps have populated the hermetic temporary catalog. The directive is
mutually exclusive with compact `team` / `agent` declarations. st2 recursively
discovers the catalog, preserves explicit `identity` and `host` authority
independent of organizational placement, and projects only declarations
resolved to the eval host. Each declaration parent remains its native
state/resource anchor. The resulting local Agent Spec vector flows unchanged
through launch admission, kickoff resolution, supervision, logging, and
declaration-driven teardown; remote-host declarations remain inert.

Admission applies `validate_for_host` strictly, then fails before spawn when
discovery is malformed or warning-bearing, when the selected local projection
is empty, duplicate, retired, root-overriding, or unrunnable, or when any
resolved local task runtime ID is empty or duplicates another local task.
Materialization warnings and backend launch errors are fatal. The kickoff
target must resolve to exactly one local agent. The eval owns one native
`CATALOG` / `ST_ROOT` and its `<catalog>/pty` registry; declarations cannot
override those roots. Local workspace renders are materialized before any
agent task starts.

Native inbox/archive paths are derived once from the admitted Agent Spec paths
and carried as frozen data; routing never re-discovers the mutable catalog.
The requester alone is an explicit eval-owned flat mailbox. Multi-agent
completion retains the worker-report-before-supervisor-confirmation ordering.
For a singleton, the eval snapshots the requester inbox before kickoff and
completes only for a newly appearing interviewer reply whose timestamp is
at-or-after the exact kickoff receipt. The filename snapshot rejects
future-dated pre-seeded messages while `>=` accepts a causally new same-ms
reply. Canonical completion is a gating judge, so a timeout cannot pass on
unrelated final-state checks alone.

Without `canonical-agents`, fixture declarations are not discovered or launched
and compact evals retain their catalog-less flat bus. This explicit opt-in keeps
ordinary fixtures inert while allowing the same canonical declaration to be
exercised in an eval and real work. Parser and admission evidence lives in
`eval_spec::tests::canonical_agents_is_bare_once_and_excludes_compact_agents` and
the `canonical_*` unit tests. Named-PTY end-to-end cases prove strict
pre-spawn refusals, poisoned ambient-root isolation, real render
materialization, frozen routing after declaration removal, singleton
completion, custom task-ID supervision/logging/teardown, and the no-opt-in
legacy control in `tests/eval_run_e2e.rs`.

## Stable identity and mutable presentation (R02, R08, R11, R13, R19, R24-R26)

The positional value in `agent "<identity>"` remains the stable Agent Spec ID.
The supported child/TOML/JSON `identity` spelling and roster JSON `identity`
field remain unchanged. Host qualification produces the existing
`<host>.<identity>` bus ID. Only that stable identity controls routing,
selection, authorization, state paths, task identity, adoption, and lifecycle.
There is no display-name resolver, stable-ID alias, or stable-ID rename command.

```kdl
agent "worker" {
  host "host"
  name "Release worker"
  description "Owns release preparation and verification."
}
```

`name` is a non-unique human label and `description` is the enduring
responsibility boundary. Omission is the only cleared representation. Name is
limited to 160 Unicode scalars and description to 1,000. Explicit empty,
surrounding-whitespace, Cc-control, U+2028/U+2029, or over-limit values are
invalid; slash and backslash remain ordinary printable characters. The Agent
Spec declaration is the sole source of truth. `<agent-dir>/name` is hard-retired:
st2 neither reads, writes, migrates, nor interprets it.

`st2 rename` and `st2 describe` accept one stable selector and either a value or
`--clear`. They edit canonical KDL only. The operation:

1. acquires the persistent exclusive
   `<catalog>/.st2/catalog-authoring.lock` before discovery;
2. resolves exactly one declaration and applies the caller-supplied `ST_AGENT`
   self/descendant guardrail when present;
3. refuses declarations explicitly marked `meta { managed-by "nix" }`,
   unsupported formats, malformed catalogs, and ambiguous targets;
4. applies one span-bounded edit, reparses and validates the candidate;
5. fsyncs a temporary under the reserved `.st2` control plane, rechecks the
   original inode/version and bytes, atomically renames it through retained
   no-follow directory capabilities, then fsyncs the declaration directory.

`ST_AGENT` is a runner-provided convention in this trusted single-operator fleet,
not an authenticated capability: a same-UID caller can alter or remove it, and
its absence selects the operator path. Nix generators must emit the ownership
marker before activating a binary with authoring commands; st2 cannot infer an
unmarked generator from KDL bytes.

The lock file is a persistent real inode and is never removed or stale-recovered.
It serializes cooperating st2 declaration readers and writers in one local POSIX
filesystem/kernel lock domain. Direct same-UID writes and independently
synchronized hosts do not participate; the source recheck detects observed
interference but is not a distributed CAS or lock service. The classified
refusal codes are an operational trusted-fleet boundary, not adversarial OS
isolation.

For each healthy managed PTY, reconciliation uses one atomic exact-task-ID
`pty metadata patch --id <task-id>` request. Every PTY receives the versioned
st2-owned tags `agent.presentation.schema=1`,
`agent.actor.path=<host>.<identity>`, and the optional
`agent.presentation.description`. The canonical PTY whose task is named `agent`
and whose ID is `<host>.<identity>` additionally receives the compatibility tags
`role=agent` and `run.role=coding-agent`, and maps `name` to native
`displayName`. Secondary PTYs retain their task-specific display convention and
clear those compatibility tags. Exec tasks receive no PTY presentation. Name is
not duplicated in tags. Clearing removes only the owned native value or tag,
and unrelated PTY metadata is preserved. Repeating the same projection is a
no-op. Failure is reported and retried by the ordinary loop, never converted
into launch, teardown, garbage collection, replacement, or flapping authority.

## Resource bindings (R20-R21)

An agent may directly declare zero or more generic Resource bindings:

```kdl
resource "work" _tag="github-issue" uri="github-issue://example/project/123"
```

The positional name is an agent-local semantic role, `_tag` is the concrete
type discriminator, and `uri` is the exact RFC 3986 absolute resource identity,
preserved byte-for-byte without normalization.
Declaration order has no meaning and binding names are unique within one
agent. The public `agent-spec` read model preserves the bindings in name order
across canonical KDL and the supported TOML/JSON forms. `st2 agents --json`
projects the same descriptors for language-neutral inspection.

st2 validates only this portable envelope. It does not define downstream type
schemas, resolve targets, infer authority from URI possession, or attach
required/optional, access, readiness, or lifecycle semantics. Those concerns
remain outside the generic binding contract. A Resource binding is declaration
metadata and is absent from task launch targets; changing only Resource
bindings adopts an already-live task without stop, replacement, or relaunch.

The unresolved-resource runtime discussion remains tracked in
[st2#60](https://github.com/compoundingtech/st2/issues/60), read-oriented
renderer integration in [st2#61](https://github.com/compoundingtech/st2/issues/61),
and the portable Agent Spec envelope in
[evals#41](https://github.com/compoundingtech/evals/issues/41).

## Transactional catalog authoring

`st2 agent digest (--spec FILE | --bundle DIR)` captures a source through
retained no-follow file descriptors and returns its authoritative digest.
`st2 agent publish --catalog ROOT (--spec FILE | --bundle DIR)
--input-sha256 HEX (--expect-absent | --expect-sha256 HEX) --json` binds
publication to that exact capture. It accepts exactly one canonical KDL `agent` node with an
explicit, path-safe host and identity. st2 no longer exposes an intent compiler:
external renderers own the transformation from human intent to exact Agent Spec
bytes or a create-only publication bundle.

The persistent `<catalog>/.st2/catalog-authoring.lock` defines one cooperative
read/write transaction domain:

```text
publisher (EX)  : snapshot input -> CAS -> full-catalog admission -> atomic publish + fsync
reader (SH)     : discover -> materialize/observe -> plan -> execute
bulk apply (EX) : root CAS -> durable stage+marker -> converge -> verify+clear
state plane     : message | context | Resource | status                    (unlocked)
```

Every publication temporary, including a not-yet-visible identity bundle, is
staged under `.st2`. Cross-directory rename therefore stays on the catalog
filesystem while a crash can leave debris only in the non-projected control
plane; declaration directories never contain writer-private leaves.

Before a declaration writer mutates the live projection it durably creates
`.st2/catalog-generation-incomplete`. Shared declaration readers and read fences
fail closed while this intent exists. After the declaration and its parent are
durable, the writer advances and fsyncs `catalog-generation`, then clears and
fsyncs the intent. The next exclusive writer recovers an orphan intent by
conservatively advancing the generation before clearing it. A crash after the
advance but before intent removal may therefore skip a generation on recovery;
the contract is monotonic change detection, not an exactly-once counter.

The lock file is a persistent real inode: replacing or removing it would split
the lock domain for a process that already has it open. Consequently, the first
coherent declaration reader may initialize exactly `.st2` and this lock even
when its requested operation later refuses. Refusal still performs no
declaration, workspace, or state mutation.

The publisher derives the destination from the captured declaration, replaces
only `agent.kdl` for a hash-authorized update, and preserves all sibling runtime
state. A bundle is create-only and is renamed from a hidden same-filesystem
stage; retry reports `unchanged` only when every projected bundle file already
matches. `--expect-absent` is idempotent for identical input.
`--input-sha256` rejects a caller/source swap and `--expect-sha256` rejects a
stale declaration writer. Full-catalog admission rejects any
structural validation error before publication. The typed result is
`published` or `unchanged`.

`st2 catalog snapshot --catalog ROOT --output DIR --json` holds SH while it
captures the canonical declaration projection: `catalog.kdl`, exact
`agents/<host>/<identity>/agent.kdl` files, static files inside those bounded
agent bundles, and every regular file in `_templates` whether or not a current
render references it. `_templates` is bounded to depth 8 below its root, 256
files, 1 MiB per file, and 32 MiB total; symlinks, hard links, special nodes,
and reserved control/state names are rejected. Runtime state, `.git`, `.st2`,
the native `pty` registry, and workspace content are excluded. A
catalog-contained Agent `workspace` or Task `cwd` is valid only when it names
that agent bundle's canonical real `.workspace`; the empty directory itself is
an exact declaration fact, while its descendants are never traversed. The
classification uses launch-equivalent variable expansion, resolves relative
values from the Agent Spec bundle, and lexically normalizes before comparing
against the logical catalog. A relative spelling is accepted only when it
normalizes to that bundle's canonical `.workspace`; unresolved variables and
every other effective relative path fail closed. The
scanner always excludes a canonical `.workspace` subtree, including an orphan
left after an agent move or removal. External workspaces remain valid and are
not part of the projection. The output is a create-only durable directory; an
identical retry is `unchanged`. Its domain-separated, path-sorted root SHA-256
covers normalized relative paths, file bytes, executable bits, and empty
workspace directory facts.

`st2 catalog diff --catalog ROOT --prepared DIR --expect-sha256 HEX --json`
holds the existing authoring lock in shared mode and performs no initialization
or publication. It projects and fully admits the coherent live catalog, rejects
unless its declaration root equals `HEX`, captures `DIR` through retained
no-follow capabilities into private temporary storage, then projects and fully
admits that capture against logical `ROOT`. Any malformed, ambiguous, stale,
symlinked, hard-linked, special, reserved, or unprojected input fails without a
partial JSON receipt.

The typed result is:

```json
{
  "schema": "st2.catalog-diff.v1",
  "catalog": "/catalog",
  "prepared": "/prepared",
  "beforeRootSha256": "<live-declaration-root>",
  "afterRootSha256": "<prepared-declaration-root>",
  "paths": [
    {
      "path": "agents/host/worker/agent.kdl",
      "kind": "modified",
      "before": { "class": "agent-spec", "executable": false },
      "after": { "class": "agent-spec", "executable": false }
    }
  ],
  "agents": [
    {
      "host": "host",
      "identity": "worker",
      "kind": "modified",
      "fields": [
        {
          "address": "/agents/host/worker/tasks/pty/agent/argv/0",
          "before": { "state": "present", "type": "string" },
          "after": { "state": "present", "type": "string" }
        }
      ]
    }
  ]
}
```

Path changes are ordered lexically and use `added`, `removed`, or `modified`.
They describe projected-fact changes: file content, executable bit,
classification, or workspace-fact presence. A missing `before` side for an
addition and a missing `after` side for a removal serialize as `null`; a
modified path has both sides. Each present side classifies the fact as
`catalog`, `agent-spec`, `render`, `template`, `static`, or `workspace-fact`.
File content remains private and only the existing aggregate declaration roots
are hashed in the receipt.
`render` means a catalog-owned bundle file consumed by a normalized render
operation, while `_templates` remains `template` even when referenced.

Agent fields lower through the shared Agent Spec model and ordered render-plan
parser. This is model-field normalization, not resolved effect normalization:
physical source paths, comments, formatting, map order, and explicit spellings
of effective defaults disappear, while accepted `workspace`, task `cwd`, and
render path strings remain exact model values. Task addresses include both kind
and name. Dynamic JSON Pointer segments use RFC 6901 escaping; for example,
environment key `A/B~C` becomes `A~1B~0C`. The address necessarily exposes the
host, identity, task/resource names, and environment/tag keys needed to locate
the field. Render operations retain their declaration order, while
`json-upsert` object keys normalize before comparison. A changed field reports
only `absent`, `default`, or `present` plus its type; inclusion in `fields`
proves the two normalized payloads differ. Payload values, lengths, and
per-field or per-agent hashes are never emitted.

`paths` describes projected-fact changes, so a formatting-only source
edit may modify `agent.kdl` while `agents` remains empty. An empty `agents`
array is normalized agent equivalence, not byte identity. The command does not
decide whether a change is safe, select agents for migration, inspect a PTY
registry, or authorize apply.

`st2 catalog apply --catalog ROOT --prepared DIR --expect-sha256 HEX --json`
rejects any prepared state/control path, symlink, special node, unprojected
file/directory, malformed declaration, nonempty prepared workspace fact,
catalog-local/default PTY root, or effective PTY-root change. Hash-CAS captures
and validates exact prepared bytes, takes EX, rechecks the canonical live root,
and either reports `unchanged` for exact equality or creates a durable
content-addressed stage before publishing the marker. Version 1 requires an
explicit PTY root outside the canonical catalog. Hash-CAS permits declared live
workspace facts and their real ancestry to contain content. It changes
declaration leaves only; desired workspace facts must already exist, and
workspace content and canonical state are never traversed, deleted, or hashed.
When an identity path is absent, its complete bundle uses an exclusive
directory rename. When its declared workspace skeleton already exists, the
durable marker fences declaration readers and marker-time state routing until
every declaration leaf has been published and verified. Applied leaves and
their parents are fsynced, the live root is re-hashed and fully admitted, then
the marker is unlinked and `.st2` is fsynced.
The catalog parent is fsynced when `.st2` is first created, including the
concurrent create/observe race. Retained source capture rejects a staging
destination contained by its source before enumerating that source.

The lock file is never removed: replacing its inode would split the transaction
domain for processes that already hold it open. Reconciliation holds SH from
discovery through execution. Validation, doctor, roster, listing,
materialization-only, targeted reconciliation, and catalog teardown take a
coherent SH snapshot. State-plane commands deliberately do not: their atomic
files remain live while a declaration is admitted.

`<catalog>/.st2/catalog-apply-incomplete` is the durable whole-catalog
transaction fence. Any presence is authoritative, including malformed content.
The reserved canonical record is:

```json
{"schema":"st2.catalog-apply-incomplete.v1","stageName":"catalog-apply-stage-<prepared-root-sha256>","expectedRootSha256":"<previous-root-sha256>","preparedRootSha256":"<prepared-root-sha256>","originalPaths":["<sorted-owned-declaration-leaf>", "..."]}
```

After taking its authoring lock, st2 refuses publication, validation,
materialization, teardown, roster, doctor, and catalog listing while the marker
exists. One-shot and selected reconcile fail explicitly. A resident supervisor
instead remains alive, reports a skipped/incomplete pass, and performs no
runtime observation or lifecycle action, avoiding a service restart storm.
Message, context, Resource, and status operations remain available. While the
marker exists they resolve canonical state from a validated address book: the
marker's original canonical agent keys union currently published real specs.
State-only directories are addressable only for original keys with recognized
real state; an incomplete or arbitrary new identity does not fall back to a
flat bus. Every host, identity, and message-box path is opened component by
component without following symlinks, and state mutations remain relative to
those retained capabilities.
A dotted bare identity is tried as
the complete local identity alongside every possible qualified bus-address
split; exactly one distinct canonical address must exist. Only real state
directories and a real regular status file can establish marker-time
addressability. Only `catalog apply --resume --catalog ROOT --json` may open an
existing marker. The closed marker and internal content-addressed stage are
sufficient recovery authority; the original prepared path and CAS precondition are
neither required nor consulted. Marker authority proves the original
precondition already passed, so recovery converges the partial live tree from
the durable desired stage and original owned-leaf list without re-enforcing
that stale precondition. Malformed or mismatched records remain fenced.
External lock execution and bypass flags are not part of the contract.

`st2 catalog bootstrap --catalog ROOT --prepared DIR --input-sha256 HEX --json`
is the create-only declaration transaction for an absent catalog. `ROOT` must
be one absent final component below an existing canonical real parent. st2
captures `DIR` through retained no-follow capabilities, verifies its declaration
root against `HEX`, admits the complete projection against logical `ROOT`, and
requires one explicit external PTY root. It materializes a 0700 sibling stage,
creates the persistent authoring lock and generation `1` inside it, takes EX on
that lock, fsyncs the complete tree, and publishes it with a capability-relative
no-replace directory rename followed by a parent fsync. Readers therefore see
absence or a complete catalog and cannot cross the already-published lock before
the parent entry is durable.

There is no bootstrap marker or resume mode: interruption before the rename
leaves `ROOT` absent, while interruption after it leaves the complete target. A
retry re-captures its source and returns `unchanged` only after taking the
existing lock, rejecting incomplete markers, proving a durable generation,
fully validating the catalog, matching the exact declaration root, proving the
locked directory remains bound to `ROOT`, and fsyncing the retained parent. A
different, malformed, symlinked, rebound, or uninitialized existing target fails
without mutation. Random sibling stages are non-authoritative and are cleaned
only by the invocation that created them; no broad orphan cleanup is permitted.

Bootstrap performs zero reads or writes below the declared PTY root. The PTY
registry has independent producers which catalog EX cannot reserve, so atomic
process adoption, continuity, or PTY-root migration requires a separate PTY
registry protocol. Bootstrap claims only atomic declaration publication.

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
  only declarations pinned to the local host. Discovery is recursive: an
  explicit `identity` and `host` pair is authoritative independent of the
  declaration's path, whose parent remains the state/resource anchor. When
  either field is omitted, the path supplies defaults and mismatches remain
  diagnostic. Dot-prefixed folders, including `.managed` and `.retired`, are
  ordinary declaration space; only `.git` and `.st2` directories at any depth,
  the catalog root's `pty` child, and a declaration parent's `resources`,
  `archive`, and `inbox` children are excluded.
  A resolved workspace-relative render destination has one coherent desired
  state across the active local fleet: byte-equivalent idempotent claims may
  share it, while incompatible
  claims fail every conflicting owner before the first workspace write.
  Targeted reconciliation checks the selected owner against the full fleet, so
  selection cannot bypass this ownership boundary.
- **R04:** Each machine schedules and reconciles only its pinned work. The st2
  loop is deterministic; exactly one declared root agent provides intelligent
  host-local supervision, bounded recovery, and escalation. Filesystem reads
  never wake reconciliation; only create, modify, rename, or remove events may
  wake it before the bounded timer. A generated companion is eligible only
  while its canonical agent task is eligible. Healthy startup launches the
  agent first and then its missing companions in the same pass. Holding or
  failing to restart the agent, or terminally parking it, suppresses companion
  launch and stops an exact generated companion proved live; explicitly
  authored sibling tasks remain independent.
- **R06:** st2 passes the complete effective task definition to the underlying
  launcher so manual and supervised restarts are equivalent. Harness readiness
  that depends on a dynamically selected account belongs to that declared
  command. In particular, reconciliation never mutates an ambient Codex config
  before launch: the command may select an account-specific `CODEX_HOME` only
  after st2 starts it. `st2 pretrust` remains an explicit operator utility for
  commands that intentionally use the ambient Claude and Codex configs.

  st2 owns `ST_AGENT` for every PTY and exec task, deriving the value as
  `<resolved-host>.<identity>` on every reconciliation. An omitted value is
  injected, an authored exact match is accepted, and a conflicting authored
  value refuses the declaration before workspace materialization or runner
  access. The derived value is part of the persisted launch environment, so
  initial launch, supervised replay, and manual PTY restart preserve the same
  identity.

  The canonical `agent` task treats a reconciler's ambient `NO_COLOR` as a
  launcher preference rather than agent policy. Unless the Agent Spec declares
  `NO_COLOR`, st2 removes it from the launch environment and records the removal
  in the PTY launch definition. An explicit Agent Spec assignment takes
  precedence. Isolation wrappers preserve both assignments and removals, so a
  manual PTY restart under a different ambient environment reconstructs the
  same effective color policy. Adoption of an already-live task remains
  non-mutating: this policy is applied only when st2 creates a generation.
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

- **Adopt-only migration fence:** A compact agent or explicit task may declare
  `lifecycle "adopt-only"`. Reconciliation adopts an already-live generation,
  but classifies a dead or absent generation as `held` without garbage
  collection or launch. Returning the declaration to the default `service`
  lifecycle is the explicit authority to resume ordinary replacement.
  `retired #true` remains the separate explicit teardown path.

- **R23:** `st2 tasks --json` is a read-only diagnostic boundary. It emits one
  `st2.task-inventory.v1` envelope for the selected host. Rows are sorted by
  agent, task, and runtime id and cover both PTY and terminal-free exec tasks.
  `complete=false` plus a non-zero exit is a closed result: a consumer must not
  turn a missing row into absence. A running row always carries a PID, creation
  time, and opaque generation id derived from stable backend evidence.

  Discovery runs before and after runtime observation. A semantic declaration
  change across those passes makes the result incomplete. The reader also
  samples `<catalog>/.st2/catalog-generation` and the incomplete marker around
  discovery and runtime observation. Every successful declaration writer
  advances and fsyncs that monotonic generation after its durable commit; apply
  does so after live verification and before clearing its marker. Even a
  completed declaration ABA is therefore incomplete. This remains an observational
  seqlock and does not serialize catalog writers. A runtime root positively absent at admission is
  empty and is not passed to its backend. An admitted PTY root that is removed
  or replaced during `pty list` is indeterminate; because the external backend
  creates an absent registry, concurrent root deletion is not a zero-write
  boundary. Malformed state, PID reuse, timeouts, duplicate ids, and observer
  failures are likewise indeterminate. Existing plain-PID exec records are
  opened read-only without following symlinks and verified by retained file
  identity, unchanged content and metadata, the final path identity, process
  start token, and record mtime without rewriting them. If that proof is
  unavailable on a supported OS, the generation remains indeterminate.

  Inventory performs no reconciliation, launch, teardown, cleanup, lifecycle
  edit, state migration, or catalog write. It does not authorize a staged
  supervisor replacement; any cutover requiring transactional declaration
  authority needs a separate protocol.

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
| declaration-space `**/agent.kdl` create/modify/remove | validate, materialize, and converge that agent and derived tasks |
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

## Quiet coordination after events (R22)

Useful work is quiet by default. Presence, status, and durable plan data are
facts. They do not cause a status message or a peer poll.

Coordination starts only after one of these events:

- An unread inbox message. DING the recipient. Process or hand off the request.
- A durable failure or real blocker. Tell the responsible supervisor. Find the cause.
  Repair the failure or escalate it.
- A completion or decision. Give the result to the agent or principal that needs it.
- A declared schedule with a name. Do that work. Do not replace the schedule
  with repeated messages or polls.

After an event, continue until you resolve the need or hand it off. Then become quiet.

The inbox uses the source contract in
[DING requirements](./01-ding/requirements.md). A normal supervisor handles
failures and blocked work. It does not continuously manage healthy agents. A
custom supervisor persona can ask for more frequent coordination. CoS is only
an example. st2 does not require or define that role, and R22 gives it no
standard authority.

R22 does not define the schedule grammar in DQ1. A schedule must have a declared
name. Transport loss can delay coordination. It does not stop independent
host-local work. Local work does not depend on a global service.

## Targeted reconciliation (R19)

`st2 up --materialize-only --task <host.agent.task>` resolves one exact local
task before writing and renders only its owning agent. `st2 up --once --task
<host.agent.task>` performs the same owner-only materialization, then inspects
PTY/exec state and executes a plan containing only that task. Unknown,
ambiguous, and wrong-host selectors refuse before writes or runner inspection;
unrelated discovery diagnostics remain visible without preventing the selected
owner/task path. A live generated companion may be adopted or retired as that
exact selected task. An active dead or absent generated companion is held: the
bounded pass cannot start its canonical agent without broadening the selector.
Explicitly authored sibling tasks retain ordinary selected-task behavior.

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
