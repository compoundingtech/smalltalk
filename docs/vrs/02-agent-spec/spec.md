# st2 Agent Spec field-change conformance specification

This draft defines desired st2 conformance behavior and st2-specific gaps. It
does not redefine the Agent Spec. A general field-change rule in this document
is proposed until the canonical evals specification and proof corpus adopt it.

## Rules common to every field

Compare normalized effects and preserve unrelated work. Prove exact ownership
before destructive action. Each host acts only on its local projection. Fence
and remove before a conflicting add. Notify surviving agents only after commit.

A dry-run makes no writes. Dry-run and quiet status name only affected IDs,
with the change, action or refusal, and what the proof covers. They include
desired and observed fingerprints and the exact incarnation when relevant.
`hold` waits without the requested lifecycle change. `refuse` makes no change
because validation or proof failed.

Each **Authoring** link points to the canonical evals Agent Spec at exact commit
`e9b53e79b05b1c0e1d7eea02db2eaba47376fe05`, which is pinned to st2
`9887b28`. It is the authoring authority, not proof of current st2 runtime
behavior. **st2 source** and **Evidence** links show this implementation on the
PR base.

## Shared declaration admission

st2 and managed publishers consume one declaration boundary from
the `agent-spec` library. They do not maintain sibling KDL parsers or reconstruct
declarations from runner-normalized `AgentSpec` values.

```text
exact UTF-8 source bytes
        |
        v
agent-spec lossless declaration parser
        |-- typed KDL tree, order, duplicates, spans, exact source
        `-- syntax and declaration-shape diagnostics
                  |
                  v
        core Agent Spec admission
          |                    |
          v                    v
   st2 catalog policy     managed publisher
          |                    |
          `---------+----------'
                    v
          digest-bound publication
```

### Strict and lossless are separate properties

The parser retains the exact source bytes and a typed tree containing every
node, argument, property, child, duplicate occurrence, source order, and source
span. An unknown provider field therefore remains available to its owning
policy; parsing never makes it valid by dropping it. Publication writes the
captured bytes, not a serialization of the typed tree.

Strict parsing rejects invalid KDL and declaration-shape errors such as an
unexpected top-level node, a task without its required name, or a reserved but
unsupported construct. It does not silently substitute a default for a value
whose supplied type or shape is invalid. Syntax failure has no document;
shape failure retains the document for diagnostics but cannot be admitted.

The shared result has this semantic shape:

```rust
struct DeclaredParse {
    document: Option<DeclaredDocument>,
    diagnostics: Vec<AdmissionDiagnostic>,
}

struct DeclaredDocument {
    source_name: PathBuf,
    source: String,
    nodes: Vec<DeclaredNode>,
    agents: Vec<DeclaredAgent>,
}
```

Convenience accessors may select a field, but the underlying collection remains
ordered and duplicate-preserving. A policy that requires uniqueness checks the
collection; it cannot inherit first-wins or last-wins behavior from an accessor.

### Policy layers add constraints without reparsing

Admission is the conjunction of ordered layers over the same immutable
`DeclaredDocument` and exact source digest:

| Layer | Owner | Decides |
| --- | --- | --- |
| syntax and declaration shape | `agent-spec` | whether a complete lossless typed document exists |
| core Agent Spec | `agent-spec` | canonical field types, uniqueness, normalization, and core invariants |
| catalog and runtime | st2 | target identity, full-catalog conflicts, host projection, and publication safety |
| managed declaration | managed publisher | managed launch, persona, Resource, provenance, and provider-specific constraints |

st2 never reports that managed policy passed, and a managed publisher never substitutes its own
answer for core admission. The managed publisher consumes the shared parse and core result, then
adds managed diagnostics. This makes a stronger managed refusal and a weaker
core acceptance explicitly different policy verdicts rather than contradictory
parses. A publication request fixes the ordered policy profile for that request;
the receipt names the profile and binds it to the candidate digest.

### Diagnostics are structured data

Every layer emits the same diagnostic envelope. Stable automation branches on
`code`, `severity`, and `layer`, never on prose.

```json
{
  "schema": "st2.agent-spec-diagnostic.v1",
  "code": "task-name-missing",
  "severity": "error",
  "layer": "declaration",
  "source": "agents/example/worker/agent.kdl",
  "span": { "offset": 42, "length": 3, "line": 3, "column": 3 },
  "fieldPath": ["agent", "worker", "pty"],
  "message": "pty task must have one positional string name",
  "help": null
}
```

`layer` is one of `syntax`, `declaration`, `core`, `catalog`, `managed`, or
`publication`. `source`, `span`, `fieldPath`, and `help` are optional only when
the failing layer has no corresponding source location. Human rendering may add
context, but JSON preserves the envelope and all causal diagnostics. Dependency
and I/O failures use their own codes and retain the failing path or operation;
they are not collapsed into an "invalid JSON" or generic admission message.

## Digest-bound publication and re-admission

Publication separates the core transaction st2 can prove under its catalog lock
from the stronger verdict owned by a managed publisher:

```text
managed publisher           st2                         live catalog
 |                           |                              |
 | parse + core + managed    |                              |
 | candidate, digest C       |                              |
 |--- publish(C, expected) ->|                              |
 |                           | acquire local authoring lock |
 |                           | re-capture candidate == C    |
 |                           | core-admit locked overlay    |
 |                           | atomic durable replace ----->|
 |                           | read back == C               |
 |                           | core re-admit published view |
 |<-- receipt(C, before, C)--| release lock                 |
 | read live == receipt C    |                              |
 | core + managed re-admit   |                              |
 | report managed success    |                              |
```

The st2 transaction performs these steps in order:

1. Capture the candidate into immutable staging, retain its exact bytes and
   typed declaration, and compute its lowercase SHA-256 source or bundle digest.
2. Acquire the catalog's exclusive local authoring lock. No cross-host lock,
   external lock service, or shared receipt is involved.
3. Re-read the current target under the lock and require the caller's exact
   absent-or-SHA-256 precondition. Require the staged digest to equal the
   caller's input digest. When that incumbent carries an ownership marker and
   the candidate would replace its bytes, require the caller to assert exactly
   that marker with `--managed-by`: an unasserted replacement refuses for the
   Nix marker, and a mismatched, unmarked, unresolvable, or malformed assertion
   always refuses — here, before any write. Creation and a byte-identical
   republication replace no owned bytes and need no assertion.
4. Overlay the staged candidate on the locked catalog snapshot and run shared
   parsing, core admission, and st2 catalog policy over that exact projection.
5. Publish by an atomic, durable file or bundle transition. Preserve exact
   candidate bytes and synchronize the containing directory before success.
6. While retaining the lock, open the published regular file without following
   symlinks, require its digest to equal the input digest, and rerun shared parse,
   core admission, and catalog policy against the published view.
7. Commit the catalog generation and return a typed receipt containing the
   policy profile, input digest, before digest when present, and verified after
   digest. Only then release the lock.

Failure before the atomic transition publishes nothing. Failure after it never
returns a success receipt: the catalog transaction restores only when its
generation record proves the exact previous bytes and target; otherwise its
durable recovery state reports an indeterminate publication and blocks dependent
work. Re-admission is not replaced by comparing bytes alone.

For a managed publication, the publisher binds its candidate verdict to the same input
digest, accepts only a matching st2 receipt, then reads the live declaration and
reruns shared parse, core admission, and its managed policy. It reports managed
success only when the live digest still equals the receipt's verified after
digest. A later writer produces `superseded`, not a false success or an
unproved rollback. st2 mutation commands that apply only core policy report a
core publication and do not claim managed admission.

## Field rules

### F01 Source form or path

Formatting, comments, order, and a source path change are `no-op` only when all
normalized fields, render plans, task IDs, fallback `cwd`, exact resolved paths,
and Resource references match. Otherwise, classify the changed effect. A
`no-op` writes and notifies nothing but does not block healing of absent or dead
work.

Authoring: [pinned discovery, identity, and host][evals-discovery]. st2 source:
[KDL parser](../../../crates/agent-spec/src/kdl_format.rs). Evidence:
[discovery](../../../crates/agent-spec/src/discovery.rs).

### F02 Agent `id` and legacy `identity`

The target `id` field is the immutable catalog-global agent ID. IDs are unique
across live and structurally archived subjects, across hosts and desired states.
Subject-creation tools generate UUIDv7. Before ID-aware routing activates,
migration assigns every live legacy declaration its existing
`<resolved-host>.<identity>` bus identity as an explicit ID without moving
runtime or declaration-anchored state. An archived declaration receives those
same bytes when unused in the combined subject set; a collision receives UUIDv7
in its declaration and tombstone. Migration also records each reassigned legacy
bus identity with the subject that kept it and the archived subject's new ID so
readers of legacy records never retype colliding bytes into the wrong subject.
Supervisor resolution uses the combined
pre-migration live-and-archived subject index. In the same catalog transition,
every reference is rewritten to the parent's migrated ID. A missing or ambiguous
reference refuses before writes with `legacy-supervisor-unresolved`; the
operator must unarchive and repair that declaration through the pre-activation
legacy authoring path, then retry. Unarchive preserves and validates the
migrated ID and refuses an unmigrated archive after activation. A frozen legacy
ID remains unchanged after host moves.

Agent-declaration membership is keyed by ID. Adding a generated ID creates a
new subject. Reintroducing an earlier ID denotes the same subject and may adopt
only state proved to belong to that ID; it never denotes a replacement.
Removing a declaration plans teardown for that exact subject. A candidate that
changes `id` at one declaration source refuses rather than inferring rename,
replacement, or state migration. Positional `identity` remains the declaration
key and address fallback; it is not immutable subject identity.

Authoring: future canonical `id` plus the pinned legacy
[discovery and identity contract][evals-discovery]. st2 source:
[`AgentSpec::{id, address, effective_id, effective_address}`](../../../crates/agent-spec/src/spec.rs),
[selection and reference resolution](../../../src/identity.rs),
[address authoring](../../../src/agent_author.rs). Evidence:
[address proof](../../../tests/agent_address.rs),
[reconciliation](../../../src/reconcile.rs).
`id` is optional in the shipped grammar, required in the target one, and no
writer emits it yet; positional `identity` is still the durable key. That gap is
fenced by
[DELTA-003](../.delta/DELTA-003-agent-address-not-implemented.md).

### F03 `host`

Each supervisor evaluates only local membership. A complete present-to-absent
change removes locally; absent-to-present adds locally. This is not migration,
and the hosts do not coordinate. Skew can cause overlap or absence. Each host
keeps its local last-known-good ownership.

Authoring: [pinned discovery and host][evals-discovery]. st2 source:
[`AgentSpec::host`](../../../crates/agent-spec/src/spec.rs). Evidence:
[host filtering](../../../src/reconcile.rs).

### F04 `type`

An omitted value and `service` have the same effect. Any other value refuses
changes to the related agent, tasks, and files before launch, write, or teardown.

Authoring: [pinned complete declaration][evals-fields]. st2 source:
[`JobType` and `RawSpec::job_type`](../../../crates/agent-spec/src/spec.rs).
Evidence: [validation](../../../src/validate.rs).

### F05 `role`

Update observable declaration metadata only. Do not change the fingerprint,
workspace files, notification state, or a healthy task.

Authoring: [pinned complete declaration][evals-fields]. st2 source:
[`AgentSpec::role`](../../../crates/agent-spec/src/spec.rs). Evidence:
[KDL lowering](../../../crates/agent-spec/src/kdl_format.rs).

### F06 `workspace`

For a healthy survivor, keep the process and commit the new live context. After
commit, write one durable event with the old and new paths, then try
transport-neutral DING through the configured adapter. Do not restart or
replace. Absent or dead work boots with the latest workspace and no notification.

Related render changes use F08 and join the same event. Explicit task `cwd` is
F11 launch drift.

Authoring: [pinned complete declaration][evals-fields]. st2 source:
[`AgentSpec::workspace`](../../../crates/agent-spec/src/spec.rs). Evidence:
[`cwd` resolution](../../../src/run.rs).

### F07 Resource `name` or `uri`

Update Resource data without changing the launch fingerprint. Notify a
survivor once after commit. New or replaced work reads the latest state at boot
and gets no change notification.

Authoring gap: the [pinned supported-field list][evals-supported-fields] and
st2 `9887b28` predate Resource bindings. Current st2 source:
[`Resource`](../../../crates/agent-spec/src/spec.rs). Evidence:
[declared Resource projection](../../../src/agents.rs).

### F08 `render {}` operation, template, or resolved target

Prove ownership for every affected local owner before writing. Conflicts refuse
all affected owners. Write changed bytes and enforce the declared mode. The
`executable=#true` property selects exact mode `0755`; absence or false selects
exact mode `0644`. A copy source mode has no effect. Mode-only drift is a
change. Notify survivors that can see the committed target. Matching bytes and
mode do not notify and do not report a materialization. Inline `file` content
uses the decoded KDL string without an added or removed newline. Deletion needs
explicit desired state and ownership and never removes a catalog source
declaration.

Authoring: [pinned render contract][evals-render]. st2 implementation and
evidence: [materializer](../../../src/materialize.rs).

### F09 Task set: `pty`, `exec`, or compact `ding`

Add only the unique missing child. Remove and clean only an old child with exact
ownership proof. A compact DING is a derived child: it starts only after its
canonical agent is already live or starts successfully in the same pass. When
that target is held, fails to restart, or is terminally parked, do not launch
the derived child and stop an exact generated child proved live. Explicit
sibling tasks, including an authored `exec "ding"`, remain independent. Do not
change unrelated siblings.

Before compact tasks are compiled, st2 captures its current absolute executable
once. A generated DING lowers to direct argv using that executable, the agent's
immutable ID through an exact-ID selector, and its effective absolute bus root.
The executable and root are separate arguments; neither shell parsing nor later
`PATH` changes can select a different target. A missing captured executable
aborts compilation before task execution. Authored command and exec source
remain unchanged, including source that happens to invoke `st2 ding`.

Authoring: [pinned compact and explicit tasks][evals-tasks]. st2 source:
[`Task` and `TaskKind`](../../../crates/agent-spec/src/spec.rs). Evidence:
[task reconciliation](../../../src/reconcile.rs).

### F10 Task `name` or explicit `id`

Remove the exact old ID and add the new ID. Do not infer one incarnation.
Report both actions, or `hold` or `refuse` when ownership proof is missing.

Authoring: [pinned explicit tasks][evals-tasks]. st2 source:
[`Task::name` and `Task::id`](../../../crates/agent-spec/src/spec.rs). Evidence:
[task reconciliation](../../../src/reconcile.rs).

### F11 Spawn inputs

Task `kind`, `command`, `argv`, explicit `cwd`, and task `env`, plus agent
`env`, `tags`, `supervisor`, and any other start input form the versioned launch
fingerprint. Absent or dead work boots with the latest inputs.

A healthy mismatch stays alive as `drifted` or `unknown`; report both
fingerprints. Command drift stays visible and does not restart automatically.
Replacement needs authority for that task and a fresh exact-incarnation check.
An `env` key named `PTY_ROOT` is only a task launch input.

Authoring: [pinned tasks][evals-tasks] and [environment][evals-environment]. The
[pinned explicit-task list][evals-task-fields] and st2 `9887b28` predate `argv`.
Current st2 source: [`AgentSpec` and `Task`](../../../crates/agent-spec/src/spec.rs).
Evidence: [spawn construction](../../../src/run.rs).

### F12 Future policy (R31)

```text
canonical catalog folder + host = supervisor scope
                            |
                            v
                      supervisor run
                            |
             park marker <--+--> unpark request
```

Agent or task `keep`, restart `attempts`, `interval`, `delay`, and `mode`, and
task `lifecycle` are future policy. Adopt healthy work. `adopt-only` holds absent
or dead work; `service` reconciles it normally. A generated companion follows
the canonical agent's effective eligibility: `adopt-only` holds it, and
exhausting a fail-mode restart policy stops or suppresses it. Invalid policy
refuses changes to the related agent and tasks.

`delay` is the minimum spacing between launches in either restart mode. In
`mode = delay`, `attempts` is a rate limit over the sliding `interval` window;
an exhausted limit defers launch until the window clears and never parks the
task. In `mode = fail`, `attempts` is a terminal launch budget counted since
the task was observed alive on every completed accounting pass for a full
`interval`. The budget remains reachable independent of reconcile cadence: a
task that repeatedly launches and dies before recovery is parked after its
declared successful-launch budget is exhausted. A completed accounting pass
that does not observe the task alive breaks accrued recovery uptime rather than
forgiving failures through silence.

A parked task is reported as parked by the typed task inventory, carrying when
it was parked, why, and what clears it, alongside an unmodified runtime
observation. The park is a supervisor decision about the runtime rather than an
observation of it, so it never replaces the observed state and never makes the
inventory envelope incomplete. Its recovery action is structured executable
argv that carries the inventory's exact canonical catalog folder and selected
host, so an operator can invoke it without ambient ownership defaults changing
the target supervisor scope.

Parking is terminal within its owning supervisor run, and its only per-task exit
is an explicit operator unpark request granted by that run. Granting one clears
exactly that task's park and restart accounting, so the next launch spends a
full budget; it never releases another parked task and never restarts a healthy
one. A request naming a task that is not parked is reported as recovering
nothing. A published park belongs to the supervisor run that made it: a
projected park whose supervisor is gone is positively not parked.

The park projection and unpark request channels share one supervisor scope: the
exact canonical catalog folder plus host. Both marker publication/observation
and request publication/consumption derive that scope identically. Supervisors
for different catalog folders on the same host cannot see or delete each
other's markers or consume each other's requests, including when the task IDs
are identical. There is no host-only compatibility channel or task-specific
namespace exception.

Authoring: [pinned complete declaration][evals-fields]. The
[pinned explicit-task list][evals-task-fields] and st2 `9887b28` predate task
`lifecycle`. Current st2 source:
[`Restart` and `TaskLifecycle`](../../../crates/agent-spec/src/spec.rs),
[`FlappingCap`](../../../src/flapping.rs), and
[`execute`](../../../src/run.rs). Evidence:
[policy planning](../../../src/reconcile.rs).

### F13 `retired #true`

Fence, stop, and clean every declared task ID with exact ownership proof, and
prevent relaunch. Retirement preserves the agent ID, removes the subject from
ordinary address routing, and releases its effective address after the retired
catalog generation becomes visible. Suspending a subject does not release its
address. Declaration removal follows F02's exact ID-keyed membership rule; a
child removal uses F09 or F10 proof.

Authoring: [pinned complete declaration][evals-fields]. st2 source:
[`AgentDesiredState`](../../../crates/agent-spec/src/spec.rs). Evidence:
[retirement planning](../../../src/reconcile.rs).

### F14 Compact agent fields

Compact `command`, `argv`, `env`, `lifecycle`, and `ding` convert to the
generated agent PTY and derived sidecar. The tasks use F09, F11, and F12;
`ding` carries the dependency on the generated agent task described there.
Compact syntax adds no other behavior.

Authoring: [pinned compact tasks][evals-tasks]. That document and st2 `9887b28`
predate compact `argv` and `lifecycle`. Current st2 source:
[KDL fields](../../../crates/agent-spec/src/kdl_format.rs). Evidence:
[`RawSpec` lowering](../../../crates/agent-spec/src/spec.rs).

### F15 Provider and ignored fields

Core st2 ignores `harness`, `model`, `persona`, `permissions`, `transport`,
`strategy`, `meta`, and provider extensions. They do not change core equality,
wake behavior, or actions. Providers may convert them into F05 through F14;
core acts only on that concrete output.

Authoring: [pinned complete declaration][evals-fields]. st2 source:
[KDL field boundary](../../../crates/agent-spec/src/kdl_format.rs). Evidence:
[`RawSpec` lowering](../../../crates/agent-spec/src/spec.rs).

### F16 Invalid or incomplete state

Refuse changes to an agent, task, or file when its desired or actual state is
unreadable, invalid, ambiguous, or conflicting. Keep last-known-good ownership
and perform no destructive action to that work. Independent agents, tasks, and
files can continue when their input and ownership proof are complete.

Authoring: [pinned validation, health, and lifecycle][evals-lifecycle]. st2
source: [`RawSpec` and `AgentSpec`](../../../crates/agent-spec/src/spec.rs).
Evidence: [validation](../../../src/validate.rs) and
[reconciliation](../../../src/reconcile.rs).

### F18 `desired-state` and `reason`

`desired-state` is one of `running`, `suspended`, or `retired`. Its omission is
running. A suspended or new-style retired declaration carries exactly one
`reason` property of 1..160 UTF-8 bytes with no surrounding whitespace,
controls, or Unicode line separators. Running carries no reason. Legacy
`retired #true` remains readable without a reason; any declaration containing
both lifecycle forms is invalid.

Running enters the ordinary field rules. Suspended and retired declarations
fence launch and materialization, then remove their exact live task set,
including generated companions. A suspended declaration is healthy when no
task is live and every retained dead record is explicitly keep-pinned. A
retired declaration is complete only when every declared task record is
absent. Resume does not override `keep`, `adopt-only`, ownership proof, or
drift/replacement policy. Inbox, archive, context, resources, and presence files
remain addressable through exact agent ID; retirement removes only ordinary
address routing.

The canonical KDL authoring form is:

```kdl
desired-state "suspended" reason="Waiting for capacity"
```

The safe authoring surface is
`st2 agent desired-state --id <agent-id> <state> [--reason ...]
[--managed-by <marker>]`. It serializes with other catalog writers, preserves
unrelated source bytes, and returns an authored-intent receipt. A transition
from retired to running or suspended validates effective-address uniqueness
against the complete prospective catalog before publication. The receipt does
not imply that reconciliation or Doctor has observed convergence.

A declaration carrying `meta { managed-by "nix" }` refuses authoring unless the
caller asserts exactly the marker that declaration carries with `--managed-by`,
which is how a projection authors the one transition its own generated source
cannot express — the seat's removal from that source. The asserted arm
additionally admits the complete prospective catalog, as an `agent publish` of
the same bytes would, and the receipt records the confirmed marker. Every other
authoring verb refuses a Nix-owned declaration unconditionally.

The exact-ID selector and reactivation validation are target behavior fenced by
[DELTA-003](../.delta/DELTA-003-agent-address-not-implemented.md); the linked
current evidence still exercises the positional legacy selector.

st2 source: [`AgentDesiredState`](../../../crates/agent-spec/src/spec.rs),
[KDL lowering](../../../crates/agent-spec/src/kdl_format.rs),
[authoring](../../../src/agent_author.rs), and
[reconciliation](../../../src/reconcile.rs). Evidence:
[parser](../../../crates/agent-spec/tests/discovery.rs),
[authoring](../../../tests/agent_desired_state.rs), and
[planning](../../../tests/reconcile.rs).

### F17 Agent `name` and `description`

Update observable declaration and runtime presentation metadata only. Neither
field participates in agent ID, address routing, selection, authorization,
state paths, launch fingerprints, workspaces, inbox events, DING, or
lifecycle. The roster reads the declaration directly; sibling `name` files are
ignored.

External harness consumers read the current Agent Spec rather than a duplicate
derived file. `st2 agents --id <agent-id> --json` returns exactly one immutable
subject or fails; in-process drivers may consume the same lowered `AgentSpec`
directly. Both paths preserve explicitly nullable name and description and
grant no lifecycle authority.

For a healthy managed PTY, patch the exact runtime task ID in place. Every PTY
receives the schema-2 owned actor-ID, current-bus-address, and
optional-description tag snapshot. Only the primary task named `agent` carries
the compatibility role and maps optional name to native display metadata.
Clearing removes only the corresponding st2-owned value. Preserve unrelated
tags and secondary display conventions. An unchanged projection is a no-op.
Failure reports and retries without stop, reap, restart, replacement, or
flapping accounting. Absent work receives the same projection at spawn.

Authoring: canonical Agent Spec presentation fields after the matching evals
change lands. st2 source: [`AgentSpec`](../../../crates/agent-spec/src/spec.rs),
[roster](../../../src/agents.rs), and [reconciliation](../../../src/reconcile.rs).
Evidence: parser, roster, exact-ID metadata, and no-restart presentation tests.


### F19 Agent `stream`

A `stream "<name>" {}` declares one agent-owned event ingress endpoint. Names
are 1..=40 lowercase alphanumeric characters with optional interior hyphens
and cannot collide with an authored task named
`stream-<name>`. The declaration contains at most one launch: `command` is an
opaque shell command, `argv` is a non-empty structured argument vector, and an
empty body means external ingress. Unknown children, including the reserved
`every`, fail admission.

A launched stream adds exactly one derived exec task named `stream-<name>` with
default runtime ID `<agent-id>.stream-<name>`. Its authored `command` or `argv`
lowers directly to that task; no stream runner or stdout line protocol is
inserted. An external-ingress stream adds no task. Adding or removing a launched
stream therefore adds or removes that exact derived companion under the owning
agent's lifecycle; changing its launch is spawn-input drift under F11. It does
not change the canonical agent task or select a delivery transport.

Authoring: canonical Agent Spec stream field after the matching evals change
lands. st2 source: [`Stream`](../../../crates/agent-spec/src/spec.rs),
[KDL lowering](../../../crates/agent-spec/src/kdl_format.rs), and
[reconciliation](../../../src/reconcile.rs). Evidence:
[`streams_are_typed_and_only_launched_streams_lower_to_derived_exec_tasks`](../../../crates/agent-spec/tests/discovery.rs)
and stream lifecycle tests in [`tests/run.rs`](../../../tests/run.rs).

### F20 Agent `address`

`address` is an optional mutable semantic alias for human routing. Its omission
uses positional `identity` as the effective legacy address. Its presence
replaces that fallback immediately; the prior value receives no alias,
redirect, or history. An explicit address is at most 255 ASCII characters and
is a dotted sequence of 1-to-63-character segments. Each segment contains only
lowercase letters, digits, and hyphens and begins and ends with a letter or
digit.

The complete prospective catalog requires effective addresses to be unique per
resolved logical host among running and suspended subjects, including
collisions between explicit addresses and identity fallbacks. Retired subjects
are non-routable and do not occupy the namespace.
Address and ID are separate typed namespaces; equal bytes do not collide.

An ordinary unpinned reference tries the complete input as a bare address and
every dotted split whose prefix is an admitted host and suffix is an effective
address in that host. A host-pinned reference tries the complete input as an
address in that host plus the qualified split whose prefix equals the pinned
host. Candidates are deduplicated by agent ID; exactly one distinct subject
must remain. An explicit typed ID bypasses this algorithm.

An address-only change updates the catalog address book and runtime metadata
without changing immutable ownership, ID-keyed supervisor edges, task IDs,
launch fingerprints, workspaces, declaration-parent state paths,
inbox/archive/context/Resource data, or a healthy runtime incarnation.

The safe authoring surface is
`st2 agent address --id <agent-id> (--clear | <address>)`. It uses the same
source-preserving, authority-scoped, stale-writer-refusing, durable catalog
transaction as F17. Clearing restores the identity fallback and is refused
when that fallback conflicts on the resolved host.

Authoring: canonical Agent Spec address after the matching evals change lands.
st2 source: future `AgentSpec::address`, address-book resolution, and
`agent_author` integration. Required evidence: parser and validation tests;
host-local collision tests including legacy fallbacks; atomic cutover and
stale-reader fencing; explicit-ID versus ordinary-address selection; no-restart
continuity across task/PTY, bus, provider, and durable-state surfaces; and
separate host/launch lifecycle controls.

Catalog and PTY roots are host runtime inputs, not Agent Spec fields. Their
migration contract is outside this VRS. See
[#85](https://github.com/compoundingtech/st2/issues/85).

## Address cutover and unsupported redirects

F20 is an atomic same-subject address-book cutover, not a moved-intent record.
The old effective address becomes unclaimed when the new catalog generation is
visible. The model has no alias, redirect, route history, expiry, cycle, or
pending-map state. An implementation that needs any old-address compatibility
must return to requirements design rather than adding an inferred fallback.

The parser and runtime do not yet support F20. The implementation gap is
recorded in the root identity delta.

## Execution order

Plan the snapshot, normalized difference, ownership, conflicts, and rollback.
Refuse before mutation when a required proof is missing. Then omit empty phases
from this fixed order:

1. **FENCE:** prevent launch of old, replaced, suspended, and retired IDs.
2. **REMOVE/QUIESCE:** stop exact old incarnations and release their resources.
3. **MATERIALIZE:** write final state for survivors and additions.
4. **ADD/BOOT:** boot missing or explicitly replaced work from final bytes.
5. **NOTIFY:** notify survivors after commit and coalesce related changes.
6. **VERIFY/REPORT:** verify exact results; roll back only when proved,
   otherwise hold or refuse.

Independent agents, tasks, and files progress separately when their input and
ownership proof are complete. The order does not coordinate hosts or authorize
replacement of drifted work.

## Current implementation gaps

- **G01, F01/F04/F15:** the prepared-catalog diff exposes normalized Agent
  Spec-model field differences, but effect-level path normalization and watcher
  consumption remain absent, and invalid KDL `type` can lower to `service`.
  [Parser](../../../crates/agent-spec/src/kdl_format.rs)
- **G02, F06-F08:** render ownership and idempotent writes exist, but dependency
  targeting and post-commit notification do not. [Materializer](../../../src/materialize.rs)
- **G03, F02/F09-F13:** ID-only adoption lacks fingerprint and incarnation
  binding. Exact removal and simultaneous remove/retire are absent. Malformed
  restart fields use defaults. See [#40](https://github.com/compoundingtech/st2/issues/40),
  the [model](../../../crates/agent-spec/src/spec.rs), and
  [reconciler](../../../src/reconcile.rs).
- **G04, F06/F11:** workspace is the current startup `cwd` fallback, not live
  context. [Runner](../../../src/run.rs)
- **G05, F03:** host filtering exists, but exact old-projection removal does
  not. No cross-host mechanism is required. [Host filter](../../../src/reconcile.rs)
- **G06, notifications:** inbox and DING exist, but reconciliation writes no
  stable change event. [Message](../../../src/message.rs) and
  [DING](../../../src/ding/mod.rs)
- **G07, planning/reporting:** st2 cannot plan and commit all related agent,
  task, and file changes as one operation, and it has no true dry-run;
  `materialize-only` writes. See [#53](https://github.com/compoundingtech/st2/issues/53)
  and [runner](../../../src/run.rs).
- **G08, F02/F20 identity migration and address routing:** the explicit `id`
  field, legacy ID migration, address parser, address-book resolver, roster and
  graph projection, and address authoring are absent.
- **G09, F17 release ordering:** source authoring requires Nix emitters to mark
  generated declarations before the compatible st2 binary is activated. The
  pinned merged PTY dependency provides the exact-ID atomic metadata-patch API;
  compatible st2 and Nix provenance adoption must still deploy as one gated cohort.
- **G10, shared admission:** st2 runner lowering, st2 publication, and
  managed admission do not yet consume one complete core-policy result and one
  structured diagnostic envelope. Publication verifies exact digests and a
  full-catalog overlay before its atomic transition, but must also re-admit the
  published view under the lock and bind the policy profile in its receipt.
- **G11, F19 canonical ownership:** st2 admits, lowers, authors, and runs the
  `stream` field, but the canonical evals `AGENT-SPEC.md` and its maintained
  acceptance cells do not yet define or prove that capability. Until the
  matching evals change lands, st2's stream implementation is ahead of the
  authoring authority rather than conformant to it.

## Acceptance cases

- Source `no-op` changes nothing while an independently dead task still heals.
- Lossless parsing preserves unknown fields, duplicates, order, exact source,
  and spans. Syntax and shape mutations produce stable structured diagnostics;
  no invalid supplied value becomes an omitted default.
- st2 core admission and managed admission consume the same parsed document
  and core verdict. A managed-only refusal is labeled as managed policy, while a
  core error blocks both without a second parse or contradictory message.
- Concurrent publication proves the caller's before and input digests under the
  catalog lock. The success receipt follows read-back and core re-admission of
  the exact published digest; managed success additionally follows publisher
  re-admission of that same live digest. A superseding writer is reported as
  `superseded` rather than success.
- Role and policy changes preserve healthy work; invalid values refuse.
- A workspace survivor gets one event. Absent or dead work boots the latest
  workspace. Explicit task `cwd` drift stays visible.
- Changed render or Resource state commits before one combined event. Unchanged,
  failed, rolled-back, periodic, new, replaced, suspended, and retired paths emit none.
- DING failure retains the event. Replay has no extra effect, and inbox events
  do not cause inbox events.
- Task add, remove, rename, and retirement affect exact runtime IDs, including
  simultaneous child removal. Legacy or partial proof holds or refuses.
- Suspension stops the exact live task set, including a derived DING, while an
  unrelated sibling retains its generation and a durable inbox message retains
  its filename. Resume uses ordinary keep/adopt-only/service rules. Doctor
  accepts only no-live plus keep-pinned dead records for suspension and still
  requires complete record absence for retirement.
- Matching fingerprints adopt. Mismatches drift. Explicit replacement proves
  the exact incarnation through fence, quiesce, materialize, and boot.
- Name and description changes update the exact Agent Spec roster row and PTY
  metadata while task ID, PID, creation identity, and generation remain
  unchanged. Exact agent-ID roster selection returns one row or fails without
  publishing duplicate presentation state. Clearing removes only owned
  presentation values. A genuine retirement still follows ordinary teardown.
- Address assignment, change, clearing, and old-address reuse prove one atomic
  before-or-after address book, fail stale routes immediately, update roster
  and PTY metadata, and preserve the healthy runtime and durable subject state.
  Host changes preserve logical agent ID but follow the existing host-placement
  lifecycle rather than the nondisruptive address rule.
- A refusal for one related set of agents, tasks, or files does not block
  independent work whose input and ownership proof are complete. Every result
  names the affected IDs, action, and what the proof covers.

[evals-discovery]: https://github.com/compoundingtech/evals/blob/e9b53e79b05b1c0e1d7eea02db2eaba47376fe05/AGENT-SPEC.md#discovery-identity-and-host
[evals-fields]: https://github.com/compoundingtech/evals/blob/e9b53e79b05b1c0e1d7eea02db2eaba47376fe05/AGENT-SPEC.md#complete-declaration-shape
[evals-supported-fields]: https://github.com/compoundingtech/evals/blob/e9b53e79b05b1c0e1d7eea02db2eaba47376fe05/AGENT-SPEC.md#L79-L101
[evals-tasks]: https://github.com/compoundingtech/evals/blob/e9b53e79b05b1c0e1d7eea02db2eaba47376fe05/AGENT-SPEC.md#compact-and-explicit-tasks
[evals-task-fields]: https://github.com/compoundingtech/evals/blob/e9b53e79b05b1c0e1d7eea02db2eaba47376fe05/AGENT-SPEC.md#L153-L157
[evals-environment]: https://github.com/compoundingtech/evals/blob/e9b53e79b05b1c0e1d7eea02db2eaba47376fe05/AGENT-SPEC.md#environment-and-expansion
[evals-render]: https://github.com/compoundingtech/evals/blob/e9b53e79b05b1c0e1d7eea02db2eaba47376fe05/AGENT-SPEC.md#render-contract
[evals-lifecycle]: https://github.com/compoundingtech/evals/blob/e9b53e79b05b1c0e1d7eea02db2eaba47376fe05/AGENT-SPEC.md#validation-health-and-lifecycle
