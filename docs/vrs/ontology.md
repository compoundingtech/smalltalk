# st2 language

This is a small disambiguation index, not a behavioral specification. Each
canonical term links to the source that owns its meaning. Follow that source
for behavior, lifecycle, ordering, and wire or file formats.

st2 inherits agent-authoring language from the
[canonical Agent Spec](https://github.com/compoundingtech/evals/blob/main/AGENT-SPEC.md#canonical-agent-specification).
Terms without an independent authority are deliberately absent.

## Canonical language

### agent

The actor st2 models.

Authority: [Agent Spec overview](https://github.com/compoundingtech/evals/blob/main/AGENT-SPEC.md#canonical-agent-specification);
[R10 agent-only identity](requirements.md)

### agent declaration

The authored KDL representation of one agent.

Authority: [Agent Spec discovery and declaration shape](https://github.com/compoundingtech/evals/blob/main/AGENT-SPEC.md#discovery-identity-and-host);
[R02 canonical KDL](requirements.md)

### agent runtime

A running instance of an agent declaration.

Authority: [declared runtime vision](vision.md);
[`AgentSpec` runtime model](../../crates/agent-spec/src/spec.rs)

### agent task

A terminal-backed or terminal-free unit declared for an agent.

Authority: [Agent Spec task contract](https://github.com/compoundingtech/evals/blob/main/AGENT-SPEC.md#compact-and-explicit-tasks);
[`Task`](../../crates/agent-spec/src/spec.rs)

### runtime resource target

A short-lived locator from one task inventory observation that tells an
external sampler where the observed task's process resources can be read.
Linux uses the exact unified cgroup-v2 path from the live process's
`/proc/<pid>/cgroup`; Darwin uses a best-effort process-tree root PID. An
unavailable target carries one bounded reason instead of a guessed or nullable
locator.

This target is not task identity, ownership, a declaration, or a retained
registry entry. Runtime ID identifies the task; PID, process generation,
cgroup path, and process-tree root locate only the observation that produced
them.

Authority: [R23 fail-closed task inventory](requirements.md);
[`task_inventory`](../../src/task_inventory.rs)

### observation locator

Ephemeral evidence used to find one observed runtime generation, never to
identify the task across generations. PID, creation time, generation ID,
runtime resource target, cgroup path, unit name, and incarnation can all be
locators in their owning contexts; only a contract that explicitly exposes one
may be used by a consumer. In the task inventory, systemd unit and scope names
are deliberately not exposed as resource locators.

Authority: [R23 fail-closed task inventory](requirements.md);
[decision 0017](.decisions/0017-task-resource-targets-are-strict-observations.md)

### launch argv

The ordered, opaque OS-string sequence comprising a task program and each of
its arguments at the launcher boundary. A platform wrapper may prepend its own
outer arguments, but it does not parse, expand, escape, or otherwise rewrite
the launch argv. This is not a shell command line. Use *provider argv* only
when referring specifically to the canonical agent provider; *launch argv*
applies to every PTY and exec task.

Authority: [R06 restartable launch definitions and R42 launch argv
transparency](requirements.md);
[host-local scheduling and supervision](spec.md#host-local-scheduling-and-supervision)

### agent ID

The explicit catalog-global immutable identifier of one logical agent subject.
New subjects use UUIDv7. Migration assigns each legacy subject its existing
host-qualified bus identity as an explicit ID without moving state. The legacy
ID's original host-looking prefix becomes opaque and does not change on a later
host move. The ID survives routing, presentation, graph, desired-state, and
runtime-incarnation changes and is never reassigned.

The declaration spelling is `id`; positional `identity` remains the legacy
declaration key and address fallback. Avoid: *agent identity* when it could mean
the subject, address, runtime incarnation, or presentation.

Authority: [R24 immutable agent ID](requirements.md);
[Agent Spec field rules](02-agent-spec/spec.md)

### agent address

The mutable semantic alias used for human routing. It is unique within one
logical host among running and suspended subjects. A retired subject is
non-routable and releases its address. An address does not encode supervisor
ancestry, filesystem placement, or immutable ownership even when its dotted
segments resemble a path.

Avoid: *agent path* — graph and filesystem paths are independent concepts;
*agent handle* — use the routing-specific term.

Authority: [R24 mutable agent address](requirements.md);
[Agent Spec field rules](02-agent-spec/spec.md)

### bus address

The host-qualified human route `<host>.<agent-address>`. An addressless legacy
declaration derives its bus address from `<host>.<identity>` until the first
explicit agent address is assigned.

Avoid: *bus ID* — the value is mutable and does not identify subject
continuity.

Authority: [R24 mutable agent address](requirements.md);
[identity and address specification](spec.md#immutable-agent-id-mutable-address-and-presentation-r02-r08-r11-r13-r19-r24-r26)

### agent name

The optional mutable, non-unique human-facing presentation label. It never
selects, routes, authorizes, or identifies an agent subject.

Authority: [R25 bounded presentation](requirements.md);
[identity and address specification](spec.md#immutable-agent-id-mutable-address-and-presentation-r02-r08-r11-r13-r19-r24-r26)

### catalog

The selected folder containing agent declarations and catalog-backed state.

Authority: [Agent Spec catalog boundary](https://github.com/compoundingtech/evals/blob/main/AGENT-SPEC.md#discovery-identity-and-host);
[catalog selection](../../src/main.rs)

### catalog root

The filesystem path selected as the catalog for a command.

Authority: [catalog path resolution](../../src/main.rs)

### supervisor scope

The ownership boundary formed by one canonical catalog folder and one host. A
supervisor run owns policy decisions and recovery requests only within this
scope.

Authority: [R31 reachable restart bounds](requirements.md);
[F12 future policy](02-agent-spec/spec.md)

### root agent

The agent assigned host-local health observation, recovery, and escalation.

Authority: [R04 root supervision](requirements.md);
[vision](vision.md)

### control plane

The replaceable `st2 up` process that reconciles host-local work.

Authority: [R11 control-plane replacement safety](requirements.md);
[`up_loop`](../../src/run.rs)

### supervisor run

One live incarnation of the control plane supervising one supervisor scope. It
owns the restart accounting and park decisions made during that incarnation.

Authority: [R31 reachable restart bounds](requirements.md);
[F12 future policy](02-agent-spec/spec.md)

### declared supervisor

The agent reference carried by a declaration for supervisory routing.

Authority: [`AgentSpec::supervisor`](../../crates/agent-spec/src/spec.rs);
[Agent Spec field](https://github.com/compoundingtech/evals/blob/main/AGENT-SPEC.md#complete-declaration-shape)

### presence

The agent-authored availability signal read from catalog-backed state.

Authority: [`status::State`](../../src/status.rs);
[R08 catalog observability](requirements.md)

### session state

st2's runtime observation of whether a task record is alive.

Authority: [`reconcile::Session`](../../src/reconcile.rs)

### observed harness state

The driver-written record of what a harness is seen doing: activity
(`idle`/`active`/`child`/`ended`, with `unknown` derived and never written),
who it is blocked on — and, when blocked on a human, what kind of ask holds
it (`permission`/`question`/`review`) — and what its input buffer holds. The observed
counterpart of the declared axes: it is not [presence](#presence) (agent-
authored availability), not [session state](#session-state) (task-record
liveness), not R08's *declared activity status*, and not R09's *working
state* (restored context).

Authority: [`harness_state`](../../src/harness_state.rs);
[05-harness-state requirements](05-harness-state/requirements.md);
[decision 0006](.decisions/0006-observed-harness-state-is-a-driver-written-catalog-record.md)

### native driver diagnostic

The current typed explanation that a native harness driver's boundary cannot
produce trustworthy evidence or complete transport work. It is advisory
evidence, not the provider's free-form error, not observed harness state, and
not authority to launch, retry, deliver, reconcile, or archive.

Authority: [05-harness-state requirements OHS-R11–OHS-R15](05-harness-state/requirements.md);
[`driver_diagnostic`](../../src/driver_diagnostic.rs)

### diagnostic stage

One closed native-driver boundary at which a diagnostic is observed:
version gate, API gate, event stream, state seed, delivery, or read-back. A
stage owns its bounded reasons and sources; a reason paired with another stage
is unknown evidence rather than a best-effort match.

Authority: [native driver diagnostic snapshot](05-harness-state/spec.md#native-driver-diagnostic-snapshot-ohs-r11ohs-r16)

### diagnostic evidence age

Reader-derived elapsed time since the current native driver diagnostic was
observed. The origin timestamp is durable; age is a projection and never file
mtime.

Authority: [native driver diagnostic snapshot](05-harness-state/spec.md#native-driver-diagnostic-snapshot-ohs-r11ohs-r16)

### harness context record

The driver-written numeric record of how full an agent's harness context window
is, how often that harness has compacted, and the adjacent facts the same
channel carries. It is the numeric sibling of
[observed harness state](#observed-harness-state)'s categorical axis: unfenced,
quantized rather than transition-guarded, and readable with its own age even
when the state axis reads `unknown`. Advisory: it authorizes nothing.

Its roster key is `context`, but the canonical term is *harness context record*.
It is not [working state](#working-state) — R09's restored durable context,
which owns the bare word *context* in st2's language.

Authority: [08-harness-context requirements](08-harness-context/requirements.md);
[decision 0014](.decisions/0014-harness-context-is-a-sibling-numeric-record.md)

### context fill

How much of a harness's context window its current conversation occupies,
published as `usedTokens`, `windowTokens`, and `usedPercent`.

`usedPercent` is **harness-native**: the number that harness itself displays to
its operator, by that harness's own rule — Claude's clamped integer over the
full window, Codex's percentage over a window with a fixed baseline removed,
pi's and omp's float, and an st2-computed ratio for OpenCode, which shows none.
The record's `harness` field is what names the rule. Two agents' `usedPercent`
values are therefore comparable as operator views, not as one measured quantity,
and a value above 100 is a real overrun rather than something to clamp.

Authority: [08-harness-context requirements HC-A04, HC-R02](08-harness-context/requirements.md);
[producer table](08-harness-context/spec.md)

### occupancy

What a context window currently holds — the numerator of
[context fill](#context-fill). Distinct from **cumulative session total**
(`sessionTotalTokens`), which is every token a session has ever spent and grows
without bound: one measured session read 2,235,329 cumulative tokens against a
258,400-token window. Every harness that publishes both publishes them side by
side under names easy to confuse; the two are never interchangeable, and only
occupancy is ever a percent's numerator.

Authority: [08-harness-context requirements HC-R16](08-harness-context/requirements.md);
[record fields](08-harness-context/spec.md)

### compaction trigger

Why a harness compacted, drawn from the closed union
`manual | auto | threshold | overflow | idle | unknown`. It is the harness's own
reason, not st2's inference: only Claude and pi put one on the compaction edge,
so `unknown` is the honest value for the other three producers rather than an
error. An unrecognized future word decodes as `unknown`, never as a definite
trigger.

Authority: [08-harness-context requirements HC-R12](08-harness-context/requirements.md);
[compaction accounting](08-harness-context/spec.md)

### Resource

An externally identified thing an agent points at, named by an absolute URI.
st2 preserves the URI's exact bytes and never normalizes them. The scheme is the
exact lookup key for an optional, catalog-declared
[Resource Profile](07-resource-profile/requirements.md); scheme meaning stays
downstream-owned and st2 ships no built-in profiles, so an unregistered scheme
stays opaque. Possession of a URI grants no authority, access, or capability.

One concept, one edge: a Resource is reached through a
[Resource binding](#resource-binding). The [linked record](#linked-record-retired)
plane that once shared the word is retired.

Authority: [R20 portable Resource bindings](requirements.md);
[issue #61 resolution](https://github.com/compoundingtech/st2/issues/61)

### Resource binding

A publisher-declared edge from one agent to one Resource, written as a
`resource` node in the agent declaration and carrying an agent-local unique
name, the URI, a required `reason`, and an optional `inactive-reason`. Bindings
are desired state: they change only through compare-and-swap publication of the
whole declaration, and a binding-only change never stops, replaces, or relaunches
healthy work.

A binding says what an agent *is for* — the work it reads and the durable state
carriers it owns. It is not a record of what the agent produced.

Authority: [`Resource`](../../crates/agent-spec/src/spec.rs) — the
live contract; [R20](requirements.md); [R21](requirements.md).
The canonical [Agent Spec Resource bindings](https://github.com/compoundingtech/evals/blob/main/AGENT-SPEC.md#agent-spec-resource-bindings)
anchor still describes the pre-#307 envelope of name and `uri` only, and would
reject the required `reason`; it is pending sync (07-resource DQ-R8).

### Resource snapshot

The one atomic profile-defined representation of a Resource binding's current
observed state. Its bytes, media type, schema identity, content digest, and
freshness form the state-first read contract. Provider webhooks, polls, and
native subscriptions are observations used to reconcile the snapshot; none is
canonical by itself.

Authority: [PROFILE-R14 atomic snapshot authority](07-resource-profile/requirements.md);
[decision 0014](.decisions/0014-resource-profiles-are-state-first-read-and-observe-capabilities.md)

### Resource invalidation

A thin, superseding notice that a binding's canonical
[Resource snapshot](#resource-snapshot) changed in a way selected for agent
attention. It carries the binding identity, current snapshot digest, and
semantic topics. It does not carry canonical snapshot bytes, a rendered
summary, or a complete provider transition.

Authority: [PROFILE-R17 semantic invalidation](07-resource-profile/requirements.md);
[Resource Profile spec](07-resource-profile/spec.md#semantic-invalidation-and-catch-up-profile-r17r20)

### semantic topic

A profile-owned stable identifier that classifies why a Resource snapshot
changed, such as `ci.failure` or `mergeability.conflict`. The profile descriptor
publishes the vocabulary and defaults. A Resource binding selector can choose
from that vocabulary but cannot create topics or change provider authority.

Authority: [PROFILE-R12 versioned profile descriptor](07-resource-profile/requirements.md);
[PROFILE-R13 validated binding selectors](07-resource-profile/requirements.md)

### pending relevance

The level-triggered fact that at least one selected Resource snapshot change has
not been delivered while delivery was unavailable. It is one boolean beside
the current and last-delivered digests, not a pending event, historical digest,
cursor, or backlog. Resume invalidates the then-current snapshot.

Authority: [PROFILE-R19 level-triggered catch-up](07-resource-profile/requirements.md);
[lifecycle prototype](07-resource-profile/.experiments/2026-08-29-smart-resource-lifecycle-prototype.md)

### linked record (retired)

An agent-owned record of something an agent produced, stored as one markdown
file with `url`, optional `title`/`tags`/`relation`, and an optional body under
`<agent-dir>/resources/links/`, written through `st2 resource add`.

**Retired.** The term is kept only so a reader who meets a surviving record
under `resources/links/` can identify it. Recording produced artifacts is
`axe work update --artifact <path> --pty <name>`. Nothing in st2 reads a linked
record, and *resource* now names only the declared plane.

Authority: [07-resource spec](07-resource/spec.md);
[decision 0011](.decisions/0011-the-linked-record-plane-is-retired.md)

### agent resource directory

The per-agent directory `<agent-dir>/resources/`, canonical for an agent's
resource files. It holds the message planes (`inbox/`, `archive/`, `sent/`),
working state and decisions (`context/`), scratch material (`tmp/`), and the
realized carriers themselves (`goal.md` and siblings).

It is not a separate meaning of *resource*. It is the **realization surface**
for [Resource bindings](#resource-binding): a binding names a carrier by URI and
the carrier's bytes live here. `dev.schickling.agent-goal://<host>/<identity>`
realizes as `resources/goal.md`; `decision-tree://<host>/<identity>` realizes as
`resources/context/decisions/`. Identity is the URI; the path is realization,
and st2 does not resolve one into the other ([R20](requirements.md)).

Authority: [`message::with_resolved_state_dir`](../../src/message.rs);
[07-resource spec](07-resource/spec.md)

### working state

An agent's restored durable context — what it is doing, what it decided, and what
it ruled out — written through `st2 context` and realized at
`resources/context/now.md`. Never a liveness or activity term; the observed
signal is [observed harness state](#observed-harness-state).

Addressed as a [Resource binding](#resource-binding) under the scheme
`working-state://<host>/<identity>`. st2 writes the carrier through
`st2 context`; resolving the scheme is a catalog's choice via an optional
[Resource Profile](07-resource-profile/requirements.md), not something st2 ships.

Authority: [R09 state continuity](requirements.md);
[`context`](../../src/context.rs);
[decision 0012](.decisions/0012-working-state-is-a-declared-carrier.md)

### restart policy

The declared rules that bound when and how an agent task may be relaunched
after it stops.

Authority: [R31 reachable restart bounds](requirements.md);
[F12 future policy](02-agent-spec/spec.md)

### restart budget

The launches available to an agent task under its restart policy before the
policy delays or parks it.

Authority: [R31 reachable restart bounds](requirements.md);
[F12 future policy](02-agent-spec/spec.md)

### parked task

An agent task whose owning supervisor run has decided not to relaunch it after
its fail-mode restart budget was exhausted. Parking is a supervisor decision,
not a session state or runtime observation.

Authority: [R23 fail-closed task inventory and R31 reachable restart bounds](requirements.md)

### operator action

An explicit instruction from an operator that requests a named control-plane
change. It is distinct from an automatic policy reaction. A projected recovery
operator action is structured executable argv and carries its supervisor
scope's exact canonical catalog folder and selected host; those axes do not
come from ambient defaults at invocation time.

Authority: [R31 reachable restart bounds](requirements.md)

### unpark request

An operator action asking the owning supervisor run to clear one parked task's
park decision and restart accounting.

Authority: [R31 reachable restart bounds](requirements.md);
[F12 future policy](02-agent-spec/spec.md)

### agent desired state

The declaration-owned whole-agent lifecycle intent: `running`, `suspended`, or
`retired`. It is distinct from presence and session state.

Authority: [R41 typed agent desired state](requirements.md);
[`AgentDesiredState`](../../crates/agent-spec/src/spec.rs)

### suspension

Reversible desired absence of an agent's live tasks while its declaration and
catalog-backed durable state remain available. Suspension is not process pause
or checkpointing.

Authority: [R41 typed agent desired state](requirements.md);
[Agent Spec field rules](02-agent-spec/spec.md)

### retirement

Terminal desired absence whose completion additionally requires every declared
task record to be collected. Legacy `retired #true` is a readable spelling.

Authority: [R41 typed agent desired state](requirements.md);
[Doctor retired absence](02-doctor/requirements.md)

### desired-state rationale

The bounded human explanation required by a new suspended or retired desired
state. It explains intent and grants no lifecycle authority of its own.

Authority: [R41 typed agent desired state](requirements.md)

### reconciliation

Comparing declared host-local work with observed runtime state to produce a
plan.

Authority: [`reconcile`](../../src/reconcile.rs);
[reconcile pass](../../src/run.rs)

### materialization

Rendering catalog-declared content into an agent workspace.

Authority: [`materialize_catalog`](../../src/materialize.rs)

### message

A durable addressed record in an agent inbox or archive.

Authority: [`message::Message`](../../src/message.rs)

### sent message

One completed ordinary Agent send or reply represented by a sender-owned row. The selected sender
is implicit and the row's directional peer is `to`.

Authority: [MESSAGE-R01 and MESSAGE-R02](03-message/requirements.md);
[`SentMessageRow`](../../crates/st2-wire/src/message.rs)

### sender history

The durable sender-owned sequence enumerated by `message sent`. Recipient inboxes, archives, and
typed service-principal request state do not supply its rows.

Authority: [MESSAGE-R01 and MESSAGE-R11](03-message/requirements.md);
[message specification](03-message/spec.md);
[`list_sent`](../../src/message.rs)

### sender history coverage

The explicit proof boundary attached to sender history: `unavailable`, `since`, or `partial`. An
empty row sequence is a complete empty result only with `since` coverage.

Authority: [MESSAGE-R03](03-message/requirements.md);
[`SentCoverage`](../../crates/st2-wire/src/message.rs)

### sent commit ledger

The sender-owned immutable chain that commits each completed Sent row. One constant-size atomic head
names the chain tip and count; complete reads verify the chain to genesis and its exact reachable
sender-row set.

Authority: [MESSAGE-R03 and MESSAGE-R08](03-message/requirements.md);
[state ownership](03-message/spec.md#state-ownership)

### idempotency key

An optional caller-supplied operation identity scoped by canonical sender, canonical recipient, and
key. It makes an exact send or reply retry return the original filename; it is not inferred from
message content.

Authority: [MESSAGE-R07 and MESSAGE-R08](03-message/requirements.md);
[retry identity](03-message/spec.md#retry-identity);
[`send_to_resolved_inbox`](../../src/message.rs)

### DING

The terminal notification that makes an agent aware of unread messages.

Authority: [DING module contract](../../src/ding/mod.rs)

### stream

A named declared event producer feeding one agent. With a `command` or `argv`
adapter form, st2 supervises its adapter; with neither, it is an external
ingress endpoint.

Authority: [STREAM-R01 declared streams](04-stream/requirements.md);
[decision 0005](.decisions/0005-streams-are-agent-nested-and-stream-named.md)

### event

One durable inbox record produced on a stream. A fact observed in the world,
not addressed speech; a distinct record kind on the shared bus transport.

Authority: [STREAM-R04 and STREAM-R06](04-stream/requirements.md);
[decision 0004](.decisions/0004-stream-events-are-a-distinct-record-kind.md)

### event-id

The mandatory producer-supplied identity of one event, deduplicated per
`(stream, event-id)`.

Authority: [STREAM-R03 mandatory event identity](04-stream/requirements.md)

### key

The optional grouping axis within a stream. Supersession collapses unread
events per `(stream, key)`.

Authority: [STREAM-R07 producer-side supersession](04-stream/requirements.md)

### adapter

The world-specific program a stream runs, declared as either a shell `command`
or direct `argv`. Packaged outside st2; it emits through `st2 event emit`.

Authority: [STREAM-A03 world logic stays outside](04-stream/requirements.md)

### stream task

The derived exec companion supervising a stream's adapter, under the same
generated-companion lifecycle as the derived DING.

Authority: [STREAM-R08 companion lifecycle](04-stream/requirements.md)

## Structure

```text
supervisor scope
`-- supervisor run
    `-- applies restart policy to agent task
        `-- spends restart budget
            `-- may decide parked task

operator action --creates--> unpark request --targets--> owning supervisor run
```

`supervisor` is the leitwort for the ownership family. `restart` is the
leitwort for policy and budget; `park` links the terminal decision to its
explicit `unpark` recovery request.

```text
native driver boundary --publishes/clears--> native driver diagnostic
       |                                      |
       `-- diagnostic stage                   `-- derives diagnostic evidence age
```

`diagnostic` is the leitwort for typed driver degradation; `observed` remains
the leitwort for harness activity evidence.

```text
harness channel --publishes--> context fill --occupies--> context window
       |                            |
       `-- compaction edge          `-- derives context fill age
              |
              `-- carries compaction trigger
```

`harness context` is the leitwort for the numeric axis, `context fill` for
occupancy, and `compaction` for the edge and its counter. *Working state* stays
outside this family entirely.

## Collision rules

- Qualify **root** as [root agent](requirements.md) or
  [catalog root](../../src/main.rs). Bare *root* does not identify
  which concept is meant.
- Qualify **supervisor** as [control plane](requirements.md) or
  [declared supervisor](../../crates/agent-spec/src/spec.rs).
- Use **supervisor run** for one live control-plane incarnation and
  **declared supervisor** for the agent reference used in supervisory routing.
  **Control plane** names the replaceable process concept, not an owning run.
- Use [presence](../../src/status.rs) for the agent-authored signal and
  [session state](../../src/reconcile.rs) for runtime liveness. Avoid
  bare *agent status* when either could be meant.
- Use [observed harness state](05-harness-state/requirements.md) for the
  driver-observed activity signal. It is a third axis beside presence and
  session state: R08's *activity status* stays the declared, agent-authored
  signal, and neither axis rewrites the other. Bare *activity* does not
  identify which is meant.
- *Working state* remains R09's restored durable context and is never a
  liveness or activity term; the observed signal is **observed harness
  state**, not *working state*.
- Use **native driver diagnostic** for typed boundary degradation and
  **observed harness state** for the harness activity projection. Neither is
  presence or session state, and a driver diagnostic never rewrites the
  observed tuple.
- Use **diagnostic stage**, **reason**, and **source** for the closed fields.
  Provider error prose may be logged, but must not become consumer branching
  vocabulary.
- Use **parked task** or **park decision** for the owning supervisor's policy
  decision. Do not use *parked* as a session state or replace the runtime
  observation with it.
- Use **recovery action** for the structured argv projected with a parked task;
  it names the exact supervisor scope rather than relying on ambient defaults.
- Use [agent ID](#agent-id) for immutable logical-subject identity,
  [agent address](#agent-address) for the mutable host-local semantic route,
  and [bus address](#bus-address) for its host-qualified form. Do not use
  *agent identity*, *agent path*, or *bus ID* when the intended axis is not
  explicit.
- **Resource** names one concept: a [Resource binding](#resource-binding) and
  nothing else. The [linked record](#linked-record-retired) plane that once shared the
  word is retired. Do not reintroduce a second sense.
- The [agent resource directory](#agent-resource-directory) is not a second
  sense either — it is where bindings are realized. Say *binding* for the
  declared edge and *carrier* for the realized bytes when both are in view.
- Use [working state](#working-state) for R09's restored durable context. The
  verb is `st2 context` and the directory is `resources/context/`, but the
  canonical term and its scheme are *working state*, not *context*.
- Bare **context** is therefore already taken. Say
  [harness context record](#harness-context-record) for the numeric harness
  axis, even though its roster key is `context`; say *working state* for R09's
  restored durable context. The two share no field, no file, and no producer,
  and neither is [observed harness state](#observed-harness-state) — that is the
  categorical axis. A record's numbers never imply a state, and a state never
  implies a fill.
- Use [context fill](#context-fill) for occupancy as a fraction and qualify it
  as **harness-native** wherever a reader might otherwise assume one formula.
  Do not call a cross-harness aggregate of `usedPercent` a measurement; it is an
  aggregate of operator views.
- Use [occupancy](#occupancy) for what the window currently holds and
  **cumulative session total** for lifetime spend. Never *total* unqualified:
  the harnesses themselves spell both `total`, and dividing the wrong one by the
  window reports several hundred percent.
- Use [compaction trigger](#compaction-trigger) for the harness's own stated
  reason. `unknown` is a value, not a failure, and st2 never infers a trigger a
  harness did not state.
- Use [message](../../src/message.rs) for the durable record and
  [DING](../../src/ding/mod.rs) for its terminal notification.
- Qualify **event**: a bare *event* in stream context is the durable
  [event](04-stream/requirements.md) record; the R13–R15 filesystem-watcher
  usage is a **watcher event**. New requirements text keeps the qualification.
- Use [key](04-stream/requirements.md) for the event-side grouping axis. The
  message-side thread/topic axis explored in
  [issue #49](https://github.com/compoundingtech/st2/issues/49) is a separate
  concept; do not merge the two by name.
- Use [stream](04-stream/requirements.md) for the declared producer and
  **stream task** for its derived companion. A *watcher stream* (R15) is
  watcher machinery, not a declared stream.

## Avoid

**seat** is not canonical st2 language. Choose by meaning: [agent](https://github.com/compoundingtech/evals/blob/main/AGENT-SPEC.md#canonical-agent-specification)
for the actor, [agent declaration](https://github.com/compoundingtech/evals/blob/main/AGENT-SPEC.md#complete-declaration-shape)
for the catalog entry, and [agent runtime](vision.md) for a running
instance. For admission, use an already-authoritative admission term only if
one exists; this index does not canonize one.

The rejected word remains visible as design leakage in
[issue #52](https://github.com/compoundingtech/st2/issues/52),
[draft PR #55](https://github.com/compoundingtech/st2/pull/55), and one
[Agent Spec phrase](https://github.com/compoundingtech/evals/blob/main/AGENT-SPEC.md#free-authoring-gate).
Those references are evidence of recurrence, not authorities for a new st2
concept.
