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

## Decision citations

Decision records are cited by their full filename stem, `NNNN-<slug>`. Bare
numbers are ambiguous for 0005, 0007, 0014, and 0015; their collisions are
recorded history, and numbers are never reused under
[`0006-observed-harness-state-is-a-driver-written-catalog-record`](.decisions/0006-observed-harness-state-is-a-driver-written-catalog-record.md).

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
is empty, duplicate, non-running, root-overriding, or unrunnable, or when any
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

## Immutable agent ID, mutable address, and presentation (R02, R08, R11, R13, R19, R24-R26)

This section is the accepted target contract. Its mutable-address half is
implemented: the grammar, host-local address uniqueness, `st2 agent address`,
and the fail-closed bare-or-qualified reference resolution. Its immutable-ID half
is not: no writer emits `id`, so the positional `<host>.<identity>` bus identity
remains the durable key every ownership, task-identity, and record surface uses.
[`0015-immutable-agent-id-and-mutable-address` Amendment 1](.decisions/0015-immutable-agent-id-and-mutable-address.md)
stages that half behind named triggers and
[DELTA-003](.delta/DELTA-003-agent-address-not-implemented.md) records what it
still requires.

An Agent Spec separates four values that the current implementation overloads:

| Concept | Declaration/runtime form | Mutability | Scope | Use |
|---|---|---|---|---|
| agent ID | required target field `id "<agent-id>"` | immutable | catalog-global | logical subject, ownership, automation, durable graph edges |
| agent address | optional `address "<address>"`; positional `identity` is the legacy fallback | mutable | unique per logical host | human routing |
| bus address | `<host>.<effective-address>` | derived | catalog | qualified human routing |
| agent name | optional `name "<name>"` | mutable | non-unique | presentation only |

Before ID-aware routing activates, catalog migration adds an explicit `id` to
every live and structurally archived declaration. It first assigns each live
legacy subject its existing host-qualified bus identity, preserving current
runtime IDs and declaration-anchored state. An archived subject receives those
same bytes when unused across the combined live-and-archived set; a collision
receives UUIDv7, recorded in both its declaration and tombstone. Migration also
durably records each reassigned legacy bus identity with the subject that kept
it and the archived subject's new ID, so readers of legacy records never retype
colliding bytes into the wrong subject. Supervisor
resolution uses the combined pre-migration live-and-archived subject index. In
the same atomic catalog transition, every reference is rewritten to its
parent's migrated ID. A missing or ambiguous reference refuses before writes
with `legacy-supervisor-unresolved`; before activation, the operator must
unarchive and repair that declaration through the ordinary legacy authoring
path, then retry migration.
Unarchive preserves the migrated ID, validates its uniqueness against the
prospective live-and-archived set, and refuses an unmigrated archive after
activation. New subjects receive UUIDv7 IDs. A legacy frozen ID's original
host-looking prefix becomes opaque: a later host move changes current placement
and bus address, not the ID.

A subject's ID survives address, name, description, supervisor, graph
placement, host placement, desired state, archival, and runtime-incarnation
changes. Retirement makes the subject non-routable and releases its address but
preserves the ID. Reintroducing the same ID denotes the same subject; a
replacement subject receives a newly generated ID.

`address` is an optional semantic alias. When omitted, positional `identity` is
the effective legacy address. When present, it is the effective address and the
legacy fallback no longer resolves as an ordinary reference. An explicit
address is at most 255 ASCII characters and is a dotted sequence of
1-to-63-character segments. Each segment contains only lowercase letters,
digits, and hyphens and begins and ends with a letter or digit.

Host qualification forms the bus address:

```kdl
agent "0199b8f4-8d3a-7c21-9a44-6f85b7320ea1" {
  id "0199b8f4-8d3a-7c21-9a44-6f85b7320ea1"
  host "dev3"
  address "dotfiles.fractal.keymap.verifier"
  name "Keymap verifier"
  description "Verifies Fractal keyboard behavior and regression evidence."
}
```

```text
agent ID       0199b8f4-8d3a-7c21-9a44-6f85b7320ea1
agent address  dotfiles.fractal.keymap.verifier
bus address    dev3.dotfiles.fractal.keymap.verifier
agent name     Keymap verifier
```

Every admitted effective address is unique within its resolved logical host
among running and suspended subjects. Retired subjects do not resolve and do
not occupy the address namespace. The complete prospective catalog enforces
both explicit-address collisions and collisions involving an addressless
declaration's identity fallback. The same address may exist on two hosts.
Address and ID are separate typed namespaces; equal string bytes do not collide.

An explicit ID selector performs only catalog-global ID lookup. An ordinary
reference uses this fail-closed candidate set:

1. when the caller pins a host, treat the complete input as an address in that
   host and also try the qualified split whose prefix equals the pinned host;
2. otherwise, treat the complete input as a bare address across the selected
   catalog and also try every dotted split whose prefix is an admitted logical
   host and whose suffix is an effective address in that host;
3. deduplicate candidates by agent ID and succeed only when exactly one subject
   remains.

This rule makes a dotted semantic address and a host-qualified bus address
decidable without guessing which dot is a separator. Absence and multiple
distinct subjects fail with address-specific diagnostics.

Ordinary CLI references and message recipients use that address algorithm.
Every agent-selecting command exposes mutually exclusive address and exact-ID
forms. A command that defaults from `ST_AGENT` consumes it through the typed ID
path; generated hooks and channels use the exact-ID form rather than passing it
to an address parser. Exact subject selection never falls through to address
lookup. Internal ownership, supervisor edges, replies, message provenance,
authoring authority, and lifecycle plans carry agent IDs, not reparsed
free-form references.

Assigning or changing `address` is one atomic address-book cutover. The old
address stops resolving as soon as the new catalog generation is visible and
may be claimed by another subject. st2 stores no rename history, redirect,
implicit alias, or time-bounded compatibility route. A stale caller fails
loudly and refreshes the roster. Catalog-generation and incomplete-transaction
fences keep each lookup on one coherent before-or-after address book.

Address changes do not alter the subject's declaration-parent state anchor,
ID-keyed supervisor edges, task IDs, launch fingerprints, workspace, inbox,
archive, context, Resource state, provider-session binding, or runtime
ownership. A healthy task keeps its PID, creation identity, and generation.
Host, graph, task, and launch changes retain the logical subject ID but still
follow their own field-specific runtime rules; logical-subject continuity does
not imply process-incarnation continuity.

`name` remains a non-unique human label and `description` remains the enduring
responsibility boundary. Omission is the only cleared representation. Name is
limited to 160 Unicode scalars and description to 1,000. Explicit empty,
surrounding-whitespace, Cc-control, U+2028/U+2029, or over-limit values are
invalid; slash and backslash remain ordinary printable characters. Neither
field participates in identity, address resolution, authorization, durable
paths, or lifecycle. The Agent Spec declaration is their sole source of truth.
`<agent-dir>/name` is hard-retired: st2 neither reads, writes, migrates, nor
interprets it.

`st2 agent address`, `st2 rename`, and `st2 describe` accept an explicit agent
ID and either a value or `--clear`. Address clearing restores the positional
identity fallback and is admitted only when that effective address remains
unique. They
edit canonical KDL only. Each operation:

1. acquires the persistent exclusive
   `<catalog>/.st2/catalog-authoring.lock` before discovery;
2. resolves exactly one declaration by immutable ID and applies the
   caller-supplied `ST_AGENT` self/descendant guardrail when present;
3. refuses declarations explicitly marked `meta { managed-by "nix" }`,
   unsupported formats, malformed catalogs, ambiguous targets, and direct ID
   mutation;
4. applies one span-bounded edit, reparses and validates the complete candidate
   catalog, including effective-address uniqueness and ID uniqueness against the
   structural archive;
5. fsyncs a temporary under the reserved `.st2` control plane, rechecks the
   original inode/version and bytes, atomically renames it through retained
   no-follow directory capabilities, then fsyncs the declaration directory.

`ST_AGENT` carries the catalog-global agent ID and remains an exact actor
selector supplied by reconciliation, not the mutable bus address. Host
placement comes from the declaration/runtime context rather than being encoded
into `ST_AGENT`. `ST_AGENT` is a trusted-fleet convention rather than an
authenticated capability: a same-UID caller can alter or remove it, and its
absence selects the operator path. Nix generators must emit the ownership
marker before activating a binary with authoring commands; st2 cannot infer an
unmarked generator from KDL bytes.

The lock file is a persistent real inode and is never removed or
stale-recovered. It serializes cooperating st2 declaration readers and writers
in one local POSIX filesystem/kernel lock domain. Direct same-UID writes and
independently synchronized hosts do not participate; the source recheck detects
observed interference but is not a distributed lock service.

Roster and graph JSON append `id`, `address`, and nullable `busAddress` while
preserving the existing field order and meanings required by their compatibility
invariants. A retired non-routable subject has null `busAddress` without making
the envelope incomplete. The existing `identity` field remains the positional
declaration key and legacy address fallback; consumers do not treat it as the
agent ID. `name` and `description` remain separate presentation fields. An
exact ID roster query returns one subject plus its current host and nullable
effective address. An ordinary address query fails on absence or ambiguity.

For each healthy managed PTY, reconciliation uses one atomic exact-task-ID
`pty metadata patch --id <task-id>` request. Compact agent lowering assigns its
canonical task the explicit runtime ID `<agent-id>`. Every long-form named task
without an explicit task ID defaults to `<agent-id>.<task-name>`, including a
task named `agent`; explicitly authored task IDs remain authoritative. This
preserves every legacy task ID and socket path because each frozen ID equals its
former bus identity and the lowering rule itself does not change. Host placement
is not separately concatenated into a default ID; host-looking bytes inside a
legacy ID are opaque. Every PTY receives the exact owned tag snapshot
`agent.presentation.schema=2`, `agent.actor.id=<agent-id>`,
`agent.actor.address=<bus-address>`, and optional
`agent.presentation.description=<description>`. Clearing a value removes only
its owned tag. Unrelated tags remain unchanged. Only the canonical compact agent
task whose task ID equals the agent ID carries the compatibility tag
`role=agent` and maps `name` to native `displayName`; other PTYs clear that role
tag and retain their task-specific display convention. Name is not duplicated
in tags. Exec tasks receive no PTY metadata.

One effective metadata delta emits one coherent `metadata_change` event, while
an unchanged patch emits none. Projection failure reports and retries but never
enters launch, teardown, garbage collection, replacement, or flapping
accounting.

## Service-principal request transport

A non-agent service that needs bounded judgment work may declare only its bus
endpoint at
`principals/<host>/<identity>/principal.kdl`:

```kdl
principal "example-ci" host="host-a"
```

The declaration creates no task, presence, persona, or Agent Spec authority.
Its content must exactly match its canonical path. `st2 request send` accepts
only such a principal as the caller and only a discovered Agent Spec as the
recipient, so a service neither impersonates an agent nor depends on the flat
orphan-recovery layout.

The caller supplies an idempotency key, a JSON body, and typed string tags.
Before the native message is published, st2 atomically reserves one random
canonical message filename and the exact request envelope under the
principal's `resources/request-state/`. Replays finish that same publication;
reuse of the key with different caller, recipient, body, or tags fails. An
agent's `st2 request reply` similarly publishes at most one typed reply to the
principal's canonical inbox. `st2 request status --json` returns the tagged
union `pending | replied`, suitable for a durable workflow to observe between
its own durable waits. st2 provides no wait loop or timer and does not turn the
request into agent lifecycle authority.

This transport is design-superseded but still normative for the shipped
implementation: [`0004-stream-events-are-a-distinct-record-kind`](.decisions/0004-stream-events-are-a-distinct-record-kind.md)
absorbs typed requests into stream events plus ordinary replies, staged behind
a deprecation window ([04-stream DQ-S4](04-stream/open-questions.md)). Until
that staging completes, this section and its invariant row remain the truth;
the drift is fenced by
[DELTA-002](.delta/DELTA-002-typed-request-absorption-pending.md).

## Resource bindings (R20-R21)

An agent may directly declare zero or more generic Resource bindings:

```kdl
resource "work" uri="github-issue://example/project/123" reason="release work item"
```

The positional name is an agent-local semantic role. `reason` explains why the
reference belongs to this agent (required; optional `inactive-reason` retains
inactive bindings). `uri` is the exact RFC 3986 absolute Resource identity,
preserved byte-for-byte without normalization. Declaration order has no meaning
and binding names are unique within one agent. A Resource URI may be referenced
by any number of agent declarations. The public `agent-spec` read model
preserves the bindings in name order across canonical KDL and the supported
TOML/JSON forms. `st2 agents --json` projects the same descriptors for
language-neutral inspection.

The generic binding envelope does not register schemes, resolve targets, infer
authority from URI possession, or attach required/optional, access, readiness,
or lifecycle semantics. Its scheme is, however, the exact lookup key for an
optional catalog-declared Resource Profile. The built-in registry is empty;
unregistered schemes stay opaque. Registered schemes resolve through the
feature-gated wasm-only SDK boundary specified by
[`07-resource-profile`](07-resource-profile/spec.md) and may supply a contained
local path plus resync notification class. Scheme semantics and profile modules
remain downstream-owned.

A Resource binding and its optional resolution are declaration/observation
metadata and are absent from task launch targets. Changing only Resource
bindings adopts an already-live task without stop, replacement, or relaunch.
Read-oriented renderer integration remains tracked in
[st2#61](https://github.com/compoundingtech/st2/issues/61), and the portable
Agent Spec envelope in
[evals#41](https://github.com/compoundingtech/evals/issues/41).

## Transactional catalog authoring

`st2 agent digest (--spec FILE | --bundle DIR)` captures a source through
retained no-follow file descriptors and returns its authoritative digest.
`st2 agent publish --catalog ROOT (--spec FILE | --bundle DIR)
--input-sha256 HEX (--expect-absent | --expect-sha256 HEX)
[--managed-by MARKER] --json` binds
publication to that exact capture. It accepts exactly one canonical KDL `agent`
node with an explicit ID plus path-safe host and identity. st2 no longer
exposes an intent compiler: external renderers own the transformation from
human intent to exact Agent Spec bytes or a create-only publication bundle.

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

The resident supervisor watches the parent of `catalog-generation` through a
dedicated non-recursive backend instance. Atomic replacement of the generation
file queues one unit wake only after the transaction has made its declaration
durable. The shared catalog lock still fences the following pass from an
in-progress writer. This constant-cost commit channel is independent of the
declaration-space watcher, so declaration subscription volume or failure cannot
delay a successful st2 publication until the full-audit timer.
The supervisor records the control-directory identity for that subscription.
If the directory is replaced, the next pass diagnoses the stale subscription
and installs a watcher on the replacement directory.

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

A hash-authorized update also passes the ownership boundary the lifecycle verb
applies, because it rewrites the same declaration wholesale. `--managed-by
MARKER` asserts the ownership marker the caller believes owns the incumbent,
and the replacement is admitted only when the incumbent's own
`meta { managed-by "..." }` names exactly that one marker; a mismatched marker,
an unmarked incumbent, an unresolvable multi-marker incumbent, and an empty or
padded assertion each refuse before any write, under the same refusal codes the
authoring verbs use. Without an assertion, only the Nix marker refuses, which
is what lets each publisher of another marker adopt the assertion on its own
schedule. Create-only publication has no incumbent to protect and a
byte-identical republication authors nothing, so neither requires an assertion;
a candidate that merely claims a marker asserts no authority, and the receipt
reports `managedBy` only for a marker the incumbent confirmed. Incumbent bytes
that are not readable as a declaration carry no ownership claim, so repairing
them stays possible for a writer who could already replace the file directly.

Host-scoped validation rejects a pty task whose session socket path would exceed the
portable `sun_path` bound. `pty` binds `<PTY_ROOT>/<session-id>.sock` and refuses a
bind over the limit, so such a task can never spawn and fails identically on every
reconcile pass, which also makes the pass result useless as a health signal for that
host. The bound is derived from the pty root resolved for the selected host, never a
fixed maximum identity length: the usable identity length is what remains of the limit
after that root. The portable 104-byte bound applies so a declaration admitted on Linux
does not fail on Darwin, and the diagnostic states the resolved path and the byte
overage so the author can shorten the identity rather than discover the failure as a
spawn error later.

A park notice whose cause is structurally unrecoverable says so instead of offering
`st2 unpark`, which would relaunch into the identical failure. The test is the same
predicate admission uses, not the wording of a spawn error.

Before returning success, publication reads the exact live declaration back
under the catalog lock, verifies its digest and bytes, and re-admits the live
catalog. `st2 validate --json` emits `st2.validate.v2`; successful JSON
publication emits `st2.agent-publish.v2`. Both identify the
`st2.core+catalog.v1` policy profile and the same `agentSpecRevision`. A clean
hermetic build uses the complete 40-hex source revision; dirty or revisionless
local builds use explicit identities that cannot compare equal to a clean
hermetic receipt. The byte-only `st2 agent digest --json` contract remains
`st2.agent-source-digest.v1` because it makes no parser or policy claim.

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

`st2 catalog snapshot --catalog ROOT --output DIR --raw-preimage --json`
externalizes a byte-oriented CAS preimage. Under the same shared lock it
structurally captures `catalog.kdl`, canonical
`agents/<host>/<identity>/agent.kdl` leaves, their bounded static bundle files,
the bounded `_templates` tree, and existing canonical `.workspace` directory
facts without parsing declaration bytes. The ordinary state/control exclusions
still apply. Every captured input must be a safe real file or directory with no
symlink or hard-link alias. No live validity, catalog-envelope, profile, or
effective PTY-root assertion exists in this mode. Its root uses the distinct
`st2.catalog-raw-preimage-root.v1` hash domain and the receipt schema is
`st2.catalog-raw-preimage-snapshot.v1`; it is not interchangeable with a strict
snapshot root. Create-only retry rechecks both the raw root and output link
counts.

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

`st2 catalog digest --catalog ROOT --prepared DIR [--json]` performs the same
retained no-follow capture and desired projection as apply without taking the
catalog lock or mutating it. Its `st2.catalog-digest.v1` receipt carries the
canonical catalog and prepared paths plus `rootSha256`, the capability passed as
`INPUT_HEX`. This command exists for invalid-incumbent repair, where semantic diff
cannot parse the live side; ordinary apply callers reuse the diff receipt's
`afterRootSha256` rather than making a redundant capture.

`st2 catalog apply --catalog ROOT --prepared DIR --input-sha256 INPUT_HEX
--expect-sha256 HEX --json`
rejects any prepared state/control path, symlink, special node, unprojected
file/directory, malformed declaration, nonempty prepared workspace fact,
catalog-local/default PTY root, or effective PTY-root change. Hash-CAS captures
and validates exact prepared bytes, rejects a captured declaration root unequal
to `INPUT_HEX` before taking EX or mutating catalog state, takes EX, rechecks the canonical live root,
and either reports `unchanged` for exact equality or creates a durable
content-addressed stage before publishing the marker. Version 1 requires an
explicit PTY root outside the canonical catalog. Hash-CAS permits declared live
workspace facts and their real ancestry to contain content. It changes
declaration leaves only; a desired workspace fact is runtime-only, so the
prepared plane need not carry its directory — a raw preimage captures none of
them — and the transaction publishes any missing one as an empty directory.
Workspace content and canonical state are never traversed, deleted, or hashed.
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
marker exists, exact agent-ID operations resolve canonical state from a
validated subject book: the marker's original canonical agent IDs union the
currently published real specs. Ordinary address operations resolve against
the corresponding coherent original-or-current address binding for that
subject and fail when the marker cannot prove one unique binding. The marker
therefore retains or content-addresses enough original declaration evidence to
recover both ID ownership and effective address; it never guesses from a
dotted string. State-only directories are addressable only for original IDs
with recognized real state; an incomplete or arbitrary new ID does not fall
back to a flat bus. Every host, agent ID, and message-box path is opened
component by component without following symlinks, and state mutations remain
relative to those retained capabilities. Only real state directories and a
real regular status file can establish marker-time addressability. Only
`catalog apply --resume --catalog ROOT --json` may open an existing marker. The
closed marker and internal content-addressed stage are
sufficient recovery authority; the original prepared path, input digest, and incumbent CAS
precondition are neither required nor consulted. The marker's `preparedRootSha256`
is the input digest already proven against retained capture before publication.
Marker authority proves both original preconditions already passed, so recovery converges the partial live tree from
the durable desired stage and original owned-leaf list without re-enforcing
that stale precondition. Malformed or mismatched records remain fenced.
External lock execution and bypass flags are not part of the contract.

`st2 catalog apply --catalog ROOT --prepared DIR --input-sha256 INPUT_HEX --expect-sha256 HEX
--raw-preimage --json` is the only writer that accepts the raw-preimage root.
It first captures and fully admits `DIR` through the ordinary strict prepared
projection. Under EX it structurally reprojects the opaque live declaration
plane and checks its raw-domain root against `HEX` before any declaration,
workspace, state, writer-temporary, marker, or stage mutation. It never parses
or semantically validates the live bytes, including their catalog envelope,
profiles, validity, or effective PTY root. A successful CAS reuses the ordinary
durable stage, generation commit, leaf publication, strict live verification,
and fsync sequence. Its receipt schema is
`st2.catalog-raw-preimage-apply.v1`; its durable marker schema is
`st2.catalog-raw-preimage-apply-incomplete.v1`. The marker schema preserves the
projection type, so source-free `--resume` emits the truthful raw-preimage
receipt after converging from the strictly validated stage. This mode owns no
policy for interpreting or transforming incumbent bytes.

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
- **R04, R31:** Each machine schedules and reconciles only its pinned work. The st2
  loop is deterministic; exactly one declared root agent provides intelligent
  host-local supervision, bounded recovery, and escalation. Filesystem reads
  never wake reconciliation; only create, modify, rename, or remove events may
  wake it before the bounded timer. A generated companion is eligible only
  while its canonical agent task is eligible. Healthy startup launches the
  agent first and then its missing companions in the same pass. Holding or
  failing to restart the agent, or terminally parking it, suppresses companion
  launch and stops an exact generated companion proved live; explicitly
  authored sibling tasks remain independent.
  Restart accounting is per task and persists across reconcile passes. Only a
  successful launch spends its declared budget. Each completed pass supplies
  the exact task IDs it proved alive; uninterrupted observed liveness may
  forgive a fail-mode budget according to the
  [restart field contract](./02-agent-spec/spec.md#f12-future-policy-r31), while an unobserved task
  loses accrued recovery uptime. A pass that exits before execution neither
  supplies a liveness observation nor closes the accounting pass.
  [PR #191](https://github.com/compoundingtech/st2/pull/191) provides cadence,
  recovery, and unobserved-pass evidence for this accounting.
- **R32:** Bounded non-interactive helpers such as `pty list --json` and
  `pty metadata patch` start in a fresh session whose leader PID is also its
  process-group ID. Standard output and error use regular temporary files, so a
  descendant inheriting those descriptors cannot hold a capture pipe open.
  After spawn, an input setup or write failure or a deadline expiry sends
  `SIGKILL` to the process group and explicitly terminates the direct child.
  st2 waits for that child until the cleanup deadline; if it cannot finish the
  wait synchronously, a background waiter takes ownership before the failure
  returns. The process-group signal reaches a descendant that outlives the
  direct child; terminating the direct child alone does not. [PR
  #202](https://github.com/compoundingtech/st2/pull/202) provides
  descendant-lifetime and direct-child-reap evidence for this contract.
- **R39:** Spawn ownership is a closed two-case lifetime choice:

  - `DetachedTask` is the existing production PTY/exec task generation.
    Detachment through the PTY daemon or `setsid` keeps it outside the
    supervisor's ownership group. Supervisor return, unwind, replacement, or
    `SIGKILL` therefore leaves its PID and creation identity unchanged; only an
    explicit down, retirement, replacement, or other reconciliation action may
    stop it.
  - `OwnedChildGroup` is an integration/evaluation harness child that must not
    outlive its parent. The owner first starts a
    watchdog as leader of a fresh process group, then joins the child to that
    exact group. Keeping the watchdog in the group pins the group ID until
    cleanup has signalled it, so a stale numeric PID cannot redirect teardown
    to a later process.

  The owner and watchdog hold opposite ends of a Unix socket pair. Only the
  owner retains the write end; it is close-on-exec and the child closes its
  inherited descriptor before exec. Normal return and Rust unwind run the
  guard's `Drop`, which sends `SIGKILL` to the exact group, closes the
  liveness channel, and reaps the direct child and watchdog. Cleanup polls for
  at most two seconds; any waits still outstanding are moved to a background
  reaper before control returns. If the parent dies before `Drop`, socket EOF
  releases the in-group watchdog, which sends `SIGKILL` to its own group.
  Descendants inherit group membership and are included. After hard parent
  death the kernel reparents and the host init/subreaper reaps the now-orphaned
  processes; synchronous wait by the dead owner is impossible.

  This hard-death realization is supported on the Unix hosts st2 targets; it
  does not claim a Windows job-object equivalent. The executable regressions
  are
  `tests/catalog_apply.rs::paused_catalog_child_is_reaped_when_test_unwinds`
  for unwind cleanup and
  `tests/eval_up.rs::owned_test_child_group_dies_after_hard_parent_death` for
  socket-EOF group cleanup. The control in
  `tests/eval_up.rs::st2_up_spec_supervises_and_respawns_a_killed_agent`, plus
  the forced-kill cases in `tests/nomad_survival.rs`, proves that an owned
  supervisor child is cleaned without absorbing the detached production task.
- **R06:** st2 passes the complete effective task definition to the underlying
  launcher so manual and supervised restarts are equivalent. Harness readiness
  that depends on a dynamically selected account belongs to that declared
  command. In particular, reconciliation never mutates an ambient Codex config
  before launch: the command may select an account-specific `CODEX_HOME` only
  after st2 starts it. `st2 pretrust` remains an explicit operator utility for
  commands that intentionally use the ambient Claude and Codex configs.

  st2 owns `ST_AGENT` for every PTY and exec task, setting it to the
  catalog-global immutable agent ID on every reconciliation. Compact agent
  lowering assigns that ID explicitly to its canonical task. Other named tasks
  without explicit IDs append `.<task-name>`, including a task named `agent`.
  The resolved host remains a separate declaration and launcher input; it is
  not separately concatenated into either derivation, and any host-looking bytes
  inside a legacy ID are opaque. An omitted `ST_AGENT` is injected, an authored
  exact match is accepted, and a conflicting authored value refuses the
  declaration before workspace materialization or runner access. The value is
  part of the persisted launch environment, so initial launch, supervised
  replay, and manual PTY restart preserve the same actor ID.

  The canonical `agent` task treats a reconciler's ambient `NO_COLOR` as a
  launcher preference rather than agent policy. Unless the Agent Spec declares
  `NO_COLOR`, st2 removes it from the launch environment and records the removal
  in the PTY launch definition. An explicit Agent Spec assignment takes
  precedence. Isolation wrappers preserve both assignments and removals, so a
  manual PTY restart under a different ambient environment reconstructs the
  same effective color policy. Adoption of an already-live task remains
  non-mutating: this policy is applied only when st2 creates a generation.
- **R42:** [Launch argv](ontology.md#launch-argv) is opaque at every
  st2-added wrapper boundary. In Linux systemd scope mode the exact outer
  command order is:

  ```text
  systemd-run --user --scope --collect --quiet --unit=<unit> --expand-environment=no -- <program> <arg>...
  ```

  `--expand-environment=no` is an outer `systemd-run` option immediately before
  the `--` separator. `<program>` and every `<arg>` are appended as their
  original OS strings, without shell rendering, dollar escaping, or
  environment substitution. Detached and degraded-detached modes remain
  `<program> <arg>...` pass-throughs with no outer command.

  `src/isolate.rs::tests::wrap_scope_disables_expansion_and_preserves_dollar_bearing_argv`
  fixes the complete wrapper order and proves literal `$HOME`, `${UNSET}`, and
  `$$` after the separator.
  `src/isolate.rs::tests::wrap_detached_modes_preserve_exact_program_and_argv`
  proves both pass-through modes. The live-system distinction and unaffected
  scope semantics are recorded in the
  [systemd scope argv experiment](.experiments/2026-09-05-systemd-scope-argv-transparency.md)
  and [`0016-systemd-scope-wrappers-disable-environment-expansion`](.decisions/0016-systemd-scope-wrappers-disable-environment-expansion.md).
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
  A non-running agent desired state remains the separate explicit teardown
  path. Suspension and retirement never use task lifecycle as an implicit
  resume or replacement authority.

- **R23:** `st2 tasks --json` is a read-only diagnostic boundary. It emits one
  `st2.task-inventory.v2` envelope for the selected host. Rows are sorted by
  immutable agent ID, task name, and runtime ID and cover both PTY and
  terminal-free exec tasks. Each row includes immutable agent ID and nullable
  current bus address; a proved non-routable retired subject has a null address
  without weakening completeness. `complete=false` plus a non-zero exit is a
  closed result: a consumer must not turn a missing row into absence. A running
  row always carries a PID, creation time, opaque generation ID derived from
  stable backend evidence, and the required `runtime.resourceTarget`.

  `resourceTarget` is internally tagged by `type` and has exactly these wire
  shapes:

  | `type` | Fields | Meaning |
  | --- | --- | --- |
  | `linuxCgroupV2` | `path` | The slash-prefixed unified-hierarchy path exactly as read from `/proc/<pid>/cgroup`; `/` is the cgroup-v2 mount root. |
  | `darwinProcessTree` | `rootPid` | The observed generation's PID, used as a best-effort process-tree root. |
  | `unavailable` | `reason` | No safe target: one of `notRunning`, `runtimeIndeterminate`, `processUnavailable`, `generationChanged`, `cgroupV2Unavailable`, or `unsupportedPlatform`. |

  Linux accepts exactly one `0::<path>` entry. It preserves the path bytes
  represented by UTF-8 text, including spaces and colons, but rejects a
  non-absolute path, NUL or carriage return, repeated separators, trailing
  separators, `.` or `..` components, duplicate unified entries, and input
  with no unified entry. It does not derive the path from a systemd unit,
  scope name, runtime ID, or naming convention.

  PTY observation captures each candidate daemon's kernel process-start token
  after the registry snapshot, then executes `pty stats --json` exactly once
  without a session argument. The returned array is socket-backed live
  evidence for every candidate. A token is admitted only when exactly one
  stats row has the task name, `process.alive=true`, the same `daemon.pid`, and
  the same `createdAt`, and a second kernel-token read equals the first.
  Resource observation then reads the token before and after the target. Exec
  compares those target-fence reads directly to the token already bound into
  its generation record. A missing process is `processUnavailable`; a stats or
  token mismatch, or a token that changes across a read, is
  `generationChanged`. Neither result exposes the candidate path or root PID.
  Darwin uses the same fence around its root PID. A bounded
  `unavailable` target is a truthful successful field and does not by itself
  make the envelope incomplete. Locators are sampled afresh on each command:
  st2 stores no scope registry, performs no resource sampling, and gives no
  target continuity guarantee. Runtime ID remains task identity.

  Discovery runs before and after runtime observation. A semantic declaration
  change across those passes makes the result incomplete. The reader also
  samples `<catalog>/.st2/catalog-generation` and the incomplete marker around
  discovery and runtime observation. Every successful declaration writer
  advances and fsyncs that monotonic generation after its durable commit; apply
  does so after live verification and before clearing its marker. Even a
  completed declaration ABA is therefore incomplete. This remains an
  observational seqlock and does not serialize catalog writers. A runtime root
  positively absent at admission is empty and is not passed to its backend. An
  admitted PTY root that is removed or replaced during `pty list` is
  indeterminate; because the external backend creates an absent registry,
  concurrent root deletion is not a zero-write boundary. Malformed state, PID
  reuse, timeouts, duplicate ids, and observer failures are likewise
  indeterminate. Existing plain-PID exec records are opened read-only without
  following symlinks and verified by retained file identity, unchanged content
  and metadata, the final path identity, process start token, and record mtime
  without rewriting them. If that proof is unavailable on a supported OS, the
  generation remains indeterminate.

  Each row retains the task-level `desiredState` (`running` or `absent`) and
  appends the declaration-level `agentDesiredState` plus
  `agentDesiredStateReason`. The legacy `retired` boolean remains a projection
  for compatible readers. `st2 agents --json` likewise appends `desiredState`
  and `desiredStateReason`; presence remains an independent observed signal.

  A believable parked-task projection is a known supervisor fault, not a
  runtime state: the row retains its unmodified runtime observation and adds
  the park time, reason, and explicit per-task recovery action while the
  envelope remains complete. That action is structured `argv` containing the
  exact canonical catalog folder and selected host observed by the inventory;
  executing it does not consult ambient defaults for either ownership axis. An
  unreadable, malformed, wrong-generation, or otherwise unbelievable park
  marker is indeterminate and makes the envelope incomplete. The reader selects
  only the marker channel owned by the exact canonical catalog folder and host
  it is observing.

- **R41/R28:** Agent lifecycle intent is one closed declaration state:
  `running`, `suspended`, or `retired`. The KDL form is a direct child such as
  `desired-state "suspended" reason="Waiting for capacity"`. Omission means
  running. New suspended and retired states require a bounded rationale;
  running forbids one. Legacy `retired #true` remains readable without a
  rationale, but old and new lifecycle syntax cannot coexist.

  Reconciliation treats both non-running states as desired task absence and
  includes derived companions. Materialization skips them. Suspension is
  converged when no task is live and only explicitly keep-pinned dead records
  remain; retirement is complete only after every task record is absent.
  Resume simply returns to ordinary task planning, preserving `keep`,
  `adopt-only`, ownership, and replacement fences. Durable messages, context,
  resources, and the declaration are not task runtime and remain available.

  `st2 agent desired-state` performs one source-preserving canonical KDL edit
  under the persistent catalog-authoring lock. The self/descendant trusted-fleet
  guardrail matches presentation authoring. Running removes
  lifecycle syntax; suspended and retired emit the canonical node. A success
  receipt proves authored intent only. Reconciliation and Doctor separately
  prove observed convergence.

  Lifecycle is the one authoring verb a declaration's own generator may use.
  Without an assertion, a Nix-owned declaration refuses exactly as before.
  `--managed-by <marker>` asserts the ownership marker the caller believes owns
  the declaration, and is admitted only when `meta { managed-by "..." }` names
  exactly that one marker; a mismatched marker, an unmarked declaration, an
  unresolvable multi-marker declaration, and an empty or padded assertion all
  refuse. The
  authority exists because a projection has exactly one transition its own
  source cannot express: the source change being projected is the seat's
  removal, so "edit the generated source instead" names an edit the operator
  already made. A marker-matched edit therefore stands in for the
  compare-and-swap `agent publish` a projection would otherwise perform, and
  carries that path's admission: the complete prospective catalog must validate
  before anything is committed, so a retirement leaving an active agent
  descended from a retired root refuses rather than landing. The receipt records
  the confirmed marker. Presentation, address, stream, and Resource authoring
  keep the unconditional refusal: none of them projects a source the generator
  cannot itself rewrite.

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

### Child-process execution (R32, R34)

Non-interactive helper shells-outs (`src/run.rs`, `src/ding/mod.rs`) share one
shape: the child is `setsid` so its pid is its process group, stdout/stderr go
to unlinked tempfiles (an escaped descendant that inherited them cannot block
cleanup), and deadline expiry kills the whole group. Wait ownership for a
killed child transfers to one shared reaper thread draining a channel — never
one detached thread per timed-out child, which accumulates without bound under
timeout storms.

Read-back is tail-capped at `CAPTURE_CAP_BYTES` (256 KiB) per stream: over-cap
streams keep their last 256 KiB and emit one diagnostic line naming the
command, stream, kept/total bytes, and cap. Memory per capture is therefore
bounded by calls × 2 × cap regardless of child behavior. `pty list --json`
parses structured output that must be whole, so it uses the explicitly named
full-stdout variant; that read is intentionally uncapped and visible at its
call site. Eval run steps and agent log dumps stream child output straight to
their catalog log files without buffering it. Rationale and rejected
alternatives: [`0007-child-output-capture-is-bounded-and-tail-preserving`](.decisions/0007-child-output-capture-is-bounded-and-tail-preserving.md).

## Catalog graph and native delivery admission (R35–R38)

Managed harness ownership is explicit. A declaration uses its typed driver or
one `session-driver "claude|codex|pi|opencode|omp"` and pairs it with exactly
one non-secret readiness declaration:

```kdl
delivery-readiness "credential"
delivery-readiness "credential" account-id="tokengate/shared"
delivery-readiness "anonymous" "model-a" "model-b" harness="omp"
```

Credential omission delegates account choice to the native driver. Anonymous
readiness has at least one model; lowering sorts and deduplicates the model set,
and `harness` must equal the effective native driver. A legacy `deliver` value
must match that same explicit driver. No command-basename inference is
admitted. A managed driver, readiness, or native delivery transport cannot
coexist with Ding; Ding remains only for opaque non-harness PTYs.

`st2 catalog graph --json` schema `st2.catalog-graph.v2` publishes
`effectiveSessionDriver` and `deliveryReadiness` separately from `runtime`. It
also publishes admitted topology:

```json
{
  "parentId": "host.parent",
  "rootId": "host.root",
  "depth": 2,
  "ancestorIds": ["host.parent", "host.root"]
}
```

A root has null `parentId`, its own `rootId`, depth zero, and an empty ancestor
array. Root counting folds retirement before the graph is built: a retired
declaration — legacy `retired #true` or `desired-state "retired"` — is outside
the org chart and never holds the root slot, while a suspended root still
counts, so a host suspending its only root stays valid and a host whose every
root is retired reports zero (#402). Duplicate identity, missing or ambiguous
parent, cycle, depth beyond 64, or a host with other than one counted root is
an error. An active agent whose chain terminates at a retired declaration is
an error too (`retired-root`): the active org chart descends from the counted
root, so one active root plus a retired root still supervising an active
worker does not validate. Every affected topology field
is null and the graph envelope has `complete: false`; downstream consumers use
these admitted facts rather than walking supervisor edges themselves. The
`declarations` view applies the same fold to legacy `retired #true`, publishing
`desiredState: "retired"`; an absent lifecycle stays null, which lowers to
running.

Retired reconciliation first attempts every live task teardown for the agent.
Only when all of those attempts succeed does it settle the declaration's whole
inbox; one failure leaves every inbox file untouched and the next pass retries
teardown plus settlement. With no live tasks, settlement proceeds immediately.
Each canonical inbox filename is linked into `resources/archive` and then
removed from the inbox. An existing archive file wins byte-for-byte, so replay
and a sync-restored duplicate converge without overwriting the receipt.
Suspended reconciliation never performs this settlement.

Retirement is where the lifecycle stops being about runtime and starts being
about catalog size, so `st2 catalog archive` adds one step past it:
`running → suspended → retired → archived`. Archival moves the whole identity
directory from `agents/<host>/<identity>` into the catalog control plane at
`.st2/archive/<host>/<identity>`, under the exclusive authoring lock and inside
one generation commit, as a same-filesystem rename. `.st2` is excluded from
catalog space at any depth, so an archived declaration is structurally
undiscoverable and is never projected by a whole-catalog transaction — it is
absent from the declaration plane, not filtered out of it. A tombstone file
beside the moved directory carries `{id, host, identity, archivedAt, reason,
archiveRoot}` and is published as one additive `archived` row in the
`st2.catalog-graph.v2` envelope; an ordinary archived identity therefore leaves
`complete: true` untouched, while an archived directory with no readable
tombstone is unexplained control-plane state and makes the envelope incomplete.
Eligibility is fail-closed and local-host only, because another host's runtime
records are not observable: canonical path, retired in either spelling, no live
or dead record for any declared task, and no remaining declaration referencing
the subject's immutable ID as `supervisor`. `st2 catalog unarchive` is the exact
reverse move; it preserves the archived ID, refuses an unmigrated archive after
ID-aware activation, and validates ID uniqueness against the prospective
live-and-archived subject set. A later transition to a routable desired state
validates prospective effective-address uniqueness.

The supervisor closes the same edge without an operator. Each `st2 up` reconcile
pass ends by archiving every local seat whose retirement outlived the catalog's
`archive-after` grace period (default `7d`; `"0"` disables the step), applying
the same fail-closed eligibility as the verb and archiving at most 25 seats per
pass so one pass stays bounded. The step runs after the pass releases its shared
lock and takes the exclusive lock non-blockingly: a contended lock skips the
step, because a pass queued behind `st2 catalog apply` would stall every live
agent's reconciliation and a due seat is still due next pass. st2 records no
timestamp for a desired-state edit, so the grace period is measured from the
supervisor's first observation of the retirement, kept in
`.st2/retired-observed.json` (`st2.catalog-retired-observed.v1`) as host →
identity → epoch millis and never in the spec. That ledger is reconciled to
exactly the currently retired seats on every pass it runs, so a seat that comes
back drops its row and a second retirement serves a fresh grace period; an
absent or unreadable ledger restarts every clock, which errs toward keeping
seats in the live catalog.

Before starting a Codex provider, st2 asks that binary to generate its
app-server JSON schemas and fingerprints only the delivery-critical projection:
every client request and server notification arm st2 uses, the exact
method-to-`params` reference for each arm, recursively referenced definitions,
and response definitions st2 reads. Only reviewed fingerprints are admitted.
Live turn, resume, and durable-receipt evidence remains a separate behavioral
check and is not inferred from the fingerprint.

## Message lifecycle

```text
sender intent → atomic inbox file → sender row → DING attempt → agent reads → archive receipt
      │            └── pending/keyed retry reuses filename ──┘       │
      └──────────── explicit partial coverage ───────────────────────┘
```

- **R05:** A matching archive filename makes an inbox copy handled; stale
  duplicates are removed without another DING. Fresh `dnd` suppresses delivery;
  `busy` does not. Failed delivery remains retryable. Sidecar restart emits a
  bounded recovery notice instead of replaying the inbox. Delivery may wake an
  agent while it is working, but an active or uncertain human composer must be
  left untouched. Unsafe delivery retries use a bounded backoff so an active
  composer cannot create a short-lived PTY probe on every inbox poll. Inbox
  reads do not wake the sidecar; only mutations bypass its bounded poll cadence.
- Sender-owned publication, crash recovery, coverage, and `message sent` are specified in
  [`03-message/spec.md`](./03-message/spec.md). Recipient publication precedes a completed sender
  row; the two directories do not share one atomic commit. Pending unkeyed intent resumes, but exact
  replay after the intent clears requires a caller-supplied idempotency key.

## State and scope

- **R08:** Presence and activity status are separate signals. The version 1
  presence record below defines presence freshness. The catalog must also
  expose the agent's current plan and step with explicit freshness so a human
  or supervising agent can understand progress without PTY inspection. The
  canonical plan-progress shape is not yet specified.
- **R09:** Durable work state is external to the model transcript and is
  restored into replacement sessions through declared workspace files and
  verified hooks.
- **R10:** Fleet identities are agents. General-purpose identity kinds are
  unsupported.

### Presence record and freshness (R08)

This section answers the presence part of DQ3. The version 1 implementation
follows this contract.

#### Version 1 record

The presence record path is `<agent-dir>/status`.

The status file uses this exact version 1 shape:

```text
available
v1 1785802653486
```

Line one is one settable state: `offline`, `available`, `busy`, `away`, or
`dnd`. `unknown` remains derived and is never written.

Line two is `v1`, one ASCII space, and an unsigned base-10 timestamp. The
timestamp counts milliseconds from the Unix epoch.

The record ends with one newline. It has no other non-empty lines. Both lines
form one atomic record.

The state remains on line one for old readers. An old reader can ignore line
two and continue to parse the state.

#### Writers and atomicity

`st2 status --set` writes the requested state with the current timestamp. A
live session owner refreshes valid non-DND records every five minutes.

A missing record becomes `available` with the current timestamp. The session
owner does not refresh `dnd`, `unknown`, or malformed records.

Every new writer emits version 1. It writes a temporary sibling and atomically
renames the complete record over the target.

A healthy periodic refresh changes the timestamp bytes. Replication can order
that content change without using the source file mtime.

#### Clock, freshness, and skew

The timestamp uses the writer's UTC wall clock. A monotonic clock cannot cross
a process restart or a host boundary.

Participating hosts must keep their UTC clocks within sixty seconds. A larger
clock error makes cross-host presence unknown.

The stale interval remains fifteen minutes. A valid record is fresh while its
age is less than fifteen minutes.

A record becomes `unknown` when its age reaches fifteen minutes. This rule
applies to every settable state, including `offline` and `dnd`.

A timestamp up to sixty seconds in the reader's future is allowed. The reader
uses zero age for this bounded future value.

A timestamp more than sixty seconds in the future produces `unknown`. The
reader does not use file mtime as a fallback for malformed version 1.

Current readers treat a future legacy status mtime as fresh because they cannot
calculate its age. A sufficiently future `dnd` mtime can therefore suppress
delivery until the reader's clock catches up. The version 1 skew rules close
this defect. They clamp only bounded future time and map larger future time to
`unknown`.

An unrecognized state still produces `offline`. A literal `unknown` produces
`unknown`. A valid state with a malformed version, timestamp, or extra line
also produces `unknown`.

The sixty-second allowance is smaller than the five-minute refresh margin. It
can extend a fresh DND hold by no more than sixty seconds.

#### Why readers use origin time

st2 does not require one catalog transport. Fabric is preferred, and Git over
SSH or a plain copy remains supported.

Git does not preserve file modification times. A checkout gives files the
checkout time. Therefore, presence freshness lives in record bytes. No
supported transport must preserve file metadata.

Replica arrival time measures transport delay, not agent activity. The
embedded writer time protects presence freshness, DND expiry, and the status
contribution to `lastActivity`.

The same reason applies to the context boot freshness check. A replica arrival
must not make old context appear fresh. This proposal does not change the
context record.

#### DND behavior

A fresh `dnd` record suppresses DING delivery. The session owner leaves its
timestamp unchanged, so an abandoned hold ages out.

A stale or invalid DND record does not suppress delivery. It reads as
`unknown`, which preserves the existing fresh-DND rule.

Replication delay cannot renew a DND hold. The reader uses the embedded write
time, not the replica materialization time.

#### Legacy rollout

A legacy record contains one valid state line and no version line. The first
version 1 reader release uses legacy file mtime for freshness.

Version 1 writers never emit a legacy record. A live non-DND session owner
upgrades its legacy record at its next five-minute refresh.

A version 1 session owner upgrades a legacy DND record once. It uses the legacy
mtime as the embedded timestamp, so the migration cannot renew the hold.

After that migration, the session owner does not refresh DND. If the legacy
mtime is unavailable, the session owner leaves the record unchanged.

A malformed two-line record is not legacy. Readers must not hide a bad version
1 record behind the legacy mtime fallback.

Fallback removal is a separate reviewed change. Removal requires all three
receipts below:

1. Every supported deployed status writer emits version 1.
2. Two fleet scans, separated by fifteen minutes, find no active legacy record.
3. No supported or retained rollback binary can emit a legacy record.

After removal, a one-line record produces `unknown`. No presence freshness
decision then depends on status file mtime.

#### `lastActivity`

For a version 1 status record, `lastActivity` uses the embedded timestamp. It
does not use the replica materialization mtime.

The reader clamps an allowed future timestamp to its current time. It omits a
malformed version 1 timestamp from the activity calculation.

Inbox and archive entries continue to use their local file mtimes. During the
legacy window, a one-line status record also contributes its file mtime.

This choice reports when the agent wrote its heartbeat. A delayed replica
cannot make an old heartbeat appear to be new agent activity.

## Provider session-start restoration (R07, R09, R17, R33)

```text
fresh durable context --\
                         +--> compose text --> jq -Rs stdin --> provider JSON stdout
boot ritual ------------/                         |
                                                  `--> construction failure signal
```

The Claude SessionStart hook reads fresh context through `st2 context read`,
wraps non-whitespace content in a source-and-agent envelope, appends the boot
ritual, and streams the complete composed text into `jq`. `jq` raw-slurps stdin
and emits `continue: true` plus
`hookSpecificOutput { hookEventName: "SessionStart", additionalContext }` on
stdout. Context bytes never occupy one process argument and are not truncated.

Missing or stale context omits the envelope while retaining the ritual. A
missing `jq` fails open with exit 0 and no output; a missing `st2` fails open
with the ritual only. These are supported degraded starts, not evidence that
context was delivered. Any other JSON-construction or delivery failure must be
distinguishable from those cases and propagate durably under R17.

Executable acceptance in `tests/claude_hooks.rs` covers the model-visible
stdout envelope, empty stderr, missing and stale context, missing dependencies,
and context larger than a platform argument limit without truncation. That
target is gated: it is in `checks.st2`'s `cargoTestFlags` and the derivation
supplies its `bash` and `jq`, so a regression fails CI. Durable failure
propagation under R17 is a separate axis and stays open as DQ5.

The owner updates this spec whenever implementation changes.
Changing [vision.md](./vision.md) or [requirements.md](./requirements.md)
requires Nathan's explicit approval.

## pi native delivery

pi has no MCP, no app-server, and no lifecycle-hook mechanism. Its integration
point is an extension loaded into the live interactive process. st2 therefore
delivers to pi through a channel that runs inside the session rather than
through the terminal.

```text
inbox --> st2 driver pi-channel --NDJSON--> pi-channel.ts --> pi.sendUserMessage()
                   ^                              |
                   `-------- delivered/failed ----'
```

`st2 driver pi-session` owns the launch. It resolves `pi-channel.ts` from this
binary's verified immutable hook set and splices `-e <path>` in immediately
after the provider program, so a declaration never carries a machine-local
path. It exports `ST2_PI_CHANNEL_BIN`, `ST2_PI_CHANNEL_CATALOG`, and
`ST2_PI_CHANNEL_IDENTITY`; the extension spawns exactly that binary rather than
resolving `st2` from `PATH`, which is what keeps a replaced control plane and
its live agents from disagreeing under R11. The wrapper also owns the
five-minute presence heartbeat for as long as its provider lives, on the same
terms as the Claude wrapper.

The channel is newline-delimited JSON in both directions. It opens with
`{"type":"hello","protocol":1,"identity":...}`; an extension that does not
understand the protocol number closes the channel rather than guessing. Each
unread inbox entry becomes one `{"type":"message","deliverAs":...,"content":
...,"meta":{...}}` frame carrying the same subject-and-body envelope the Claude
channel uses. The inbox remains the durable source of truth: the channel holds
only an ephemeral set of filenames delivered in its current lifetime, and a
restart rescans. EOF on stdin is the session boundary.

pi has no session-start hook, so the `hello` frame also carries
`sessionContext`: the same restored working state, boot ritual, and
unread-inbox listing that `$ST_HOOKS/codex-session-start.sh` composes, in the
same order, composed here in Rust. The extension awaits that frame inside
`session_start`, which orders restoration ahead of the boot turn rather than
after it, and a fresh channel is opened for `/new`, `/resume`, and `/fork`, so
a replaced session is restored on the same terms.

`deliverAs` is st2's decision, not the extension's, so delivery behaviour is a
Rust change with one place to make it. The shipped value is `steer`: the
earliest point pi accepts input without discarding the running turn, and the
same choice the Codex native path makes when it routes an active turn to a
steer. Measured against a live provider, the boundary is one tool call, not one
job — each tool call and its result are their own assistant message, so a steer
sent mid-job is injected in the same millisecond as the next tool result.
Latency is bounded by the in-flight call's remaining duration. Whether displaced
work resumes is the model's choice, not a harness guarantee. Holding until the agent settles would defer
delivery inside pi where st2 cannot observe it, which is what the "`busy`
delivers immediately" rule exists to prevent. Per-message selection between
steer and queue is tracked in #277.

A managed pi seat runs with `PI_OFFLINE=1` and `PI_SKIP_VERSION_CHECK=1` unless
its declaration already sets them: a supervised agent must not self-update or
make its boot latency depend on the network. The extension unexports
`ST2_PI_CHANNEL_*` once it has read it, because pi places its environment in
front of every tool child, and eval seats scrub `PI_*` for the same reason they
scrub `CLAUDE_*` and `CODEX_*`.

st2 pins no pi version. pi publishes none to pin — no changelog, stability
policy, compat field, or API version constant — but it does publish its
TypeScript declarations, and those govern this coupling.
`checks.pi-extension-types` type-checks the shipped extension against a pinned
pi release at build time, which is what makes the asset's otherwise-erased
`import type` load-bearing. This follows the repo's existing rule that st2 validates
where skew fails silently (`pty`, the Codex app-server schema) and not where it fails
loudly (`claude`); the extension's one silent surface was its idle proof, and using
that proof without calling it is now a build error.

A pi agent never enters the DING path. `deliver` and `ding` are mutually
exclusive in a declaration, so no PTY write is authorized for a pi seat and the
synchronous composer proof is not consulted. Workspace trust is a launch flag
(`pi -a`), so nothing in the operator's ambient pi configuration is mutated and
there is no pi analogue of the pre-boot trust batching Claude requires.

The measured basis for this design, including the runs that fixed `steer` and
the modal case, is recorded in
[2026-08-18-pi-harness-integration](./.experiments/2026-08-18-pi-harness-integration.md);
the ruling is
[`0005-pi-delivers-natively-through-an-injected-extension`](./.decisions/0005-pi-delivers-natively-through-an-injected-extension.md).

## Event contracts (R13–R15, R40)

An event is evidence, not permission to mutate runtime state. The classifier
maps each accepted path and kind to one consumer. A resident catalog wake
starts one serialized full-catalog pass; discovery and runtime observation then
compute the desired-versus-actual delta, and an empty plan performs no launch,
teardown, materialization, or catalog write.

Watchers are deny-by-default. The classifier/action contract is:

| Event | Minimal action |
| --- | --- |
| `.st2/catalog-generation` create/modify/replace | queue one serialized full-catalog reconcile after a cooperative commit |
| root `catalog.kdl` create/modify/remove | queue one serialized full-catalog reconcile |
| declaration-space `**/agent.kdl` create/modify/remove or declaration-directory topology change | queue one serialized full-catalog reconcile |
| referenced `_templates/**` mutation | queue one serialized full-catalog reconcile |
| inbox create/archive/remove | DING consumer only; supervisor no-op |
| plan/resource/status mutation | specialized consumer only; supervisor no-op |
| PTY/exec/log/PID/socket/lock/temp/backup/read/open/unknown | no-op |

The catalog-generation watcher owns a separate backend instance and one
non-recursive control-directory subscription. It revalidates that directory's
identity after every pass and reinstalls a stale subscription. The declaration
watcher owns shallow subscriptions only for declaration-space directories; it
prunes `.git`, `.st2`, PTY state, inboxes, archives, and Resource payloads. Both
channels forward unit wakes to the same single-threaded loop. A burst is drained
before one pass, and a pass never overlaps another pass.

Watcher setup or callback failure is diagnosed and queues a conservative audit
when a callback exists. The independent channel remains live. Startup, timer,
watcher overflow/loss, and ambiguity are bounded full-audit fallbacks.
Executable proof covers a post-commit generation replacement, generation-watch
reinstallation after control-directory replacement, direct atomic bundle
publication at production catalog scale, root configuration and declaration
wakes, negative runtime/bus events, disconnected-channel cadence, and an idle
supervisor that does not wake on its own reads.

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
  scheduler requirements. Three of the four unspecified items now have accepted
  answers in the [stream subsystem](04-stream/requirements.md): the event inbox
  is the ordinary agent inbox, deduplication is `(stream, event-id)` per
  recipient, and receipts are the `created | deduplicated` emit receipt plus
  archive filenames. The `schedule` KDL shape, its relationship to a timer
  stream, and the approval above remain open; `every` on a stream stays
  rejected so this question is not resolved by implication.
- **DQ2 Safe DING delivery:** Bounded observation now replaces the fixed
  paste-to-Return delay: maintained Codex and Claude composers must be
  positively empty before paste and show the exact staged notice twice before
  a separate Return. Human, modal, active, changed, timed-out, and unknown
  states fail closed, with staged-payload ownership preventing duplicate paste.
  This measured screen heuristic is still not an evented proof and renderer
  changes may defer delivery. Resolve the remaining gap with a stronger evented
  signal or other measured classifier; a small on-device model is an optional
  experiment, not a required architecture. The
  [05-harness-state](05-harness-state/spec.md) pi producer is the first
  evented signal for any harness (the injected extension's positive idle
  observation); it informs observability only — delivery keeps its own
  evidence, and no DING path reads the observed-state record.
- **DQ3 Remaining catalog agent state:** The R08 presence record above
  defines the presence path, schema, freshness, and atomic update rules.
  The *observed* axis is now specified in
  [05-harness-state](05-harness-state/requirements.md) (decision 0006): a
  driver-written record with its own staleness rules, distinct from declared
  presence and from R08's declared activity status. *Declared* activity
  status, current plan, and current plan step remain undefined. The
  stale-state gate is addressed for the observed axis; the
  supervisor-following gate is not
  ([DQ-H5](05-harness-state/open-questions.md)) — prove both before adding
  any of these shapes to `AGENT-SPEC.md`.
- **DQ4 Relaunch boundary (R29-R30):** Preserve R11's nondisruptive adoption
  while making launch drift visible. For each declared task, derive the desired
  launch fingerprint from a deterministic, versioned encoding of only:
  backend kind, lowered shell source or direct argv, resolved working directory,
  and the st2-managed plus declared effective environment. Tags, descriptive
  metadata, unrelated inherited environment, file contents, and other boot-time
  snapshots are not part of this fingerprint.

  As part of its own successful task launch, st2 records the observed
  fingerprint together with that launch's exact runtime identity and creation
  incarnation. The observed fingerprint is trustworthy only while the current
  live runtime exactly matches that binding. Inspection then reports:

  | State | Meaning | Healthy-task action |
  | --- | --- | --- |
  | `converged` | desired and bound observed fingerprints match | adopt |
  | `drifted` | desired and bound observed fingerprints differ | adopt and report drift |
  | `unknown` | the observed binding is missing or does not match the live runtime | adopt and report unknown |

  `unknown` includes a healthy legacy or externally adopted runtime, as well as
  a manual PTY restart or external child replacement whose runtime identity or
  creation incarnation no longer matches st2's launch record. Stale observed
  metadata is never reused for the new incarnation. Catalog publication,
  supervisor restart, metadata edits, and launch-field edits do not implicitly
  disrupt any healthy task.

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
  2. a healthy legacy runtime reports `unknown` and remains unchanged, and a
     manual or external restart with a changed runtime identity or creation
     incarnation cannot reuse the prior observed fingerprint and reports
     `unknown`;
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
- **DQ5 Session-start failure receipt (R17, R33):** The hook has explicit
  fail-open results for missing enrichment dependencies, but an unexpected
  `jq` construction failure can still leave the provider without context or a
  durable supervisor-visible receipt. Resolve the gap by defining and proving
  a failure signal that does not block provider startup, is distinguishable
  from an ordinary cold start, and reaches the responsible supervisor under
  R17.
