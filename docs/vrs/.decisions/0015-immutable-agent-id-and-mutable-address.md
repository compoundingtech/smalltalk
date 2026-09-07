# Immutable agent ID is separate from mutable address and presentation

Status: accepted

Design confirmed by Johannes on 2026-09-05 through the issue #401 interview.
Nathan confirmed the graph-subject model in the design discussion supplied by
Johannes: the subject keeps a stable ID while its semantic name changes.

## Context

Agent Spec currently makes the positional `identity` serve as logical subject,
human route, bus address, durable-state key, and task/PTY prefix. `name` and
`description` can change without disrupting a process, but neither routes.

Quick subject creation happens before the subject has enough context to choose a
good semantic route. A provisional identity therefore becomes permanent, or a
later semantic correction creates a new subject and strands its routing,
runtime, inbox, archive, and context. A representative desired refinement is
`dev3.dotfiles.fractal.help-key.verifier` to
`dev3.dotfiles.fractal.keymap.verifier` without changing the logical agent.

PTY provides partial prior art. It separates an immutable session ID from a
mutable display name, resolves human references through the display name, and
keeps metadata changes nondisruptive. PTY does not provide the distinct unique
semantic route required here, nor route history, cross-host identity, or
catalog transactions.

## Decision

Each logical agent subject has one explicit catalog-global immutable **agent
ID**. New subjects use UUIDv7. Before ID-aware routing activates, migration adds
an `id` field to every live and structurally archived legacy declaration. A live
subject receives its existing host-qualified bus identity, preserving current
runtime identifiers. An archived subject receives those bytes when unused
across the combined live-and-archived subject set; an archived collision
receives UUIDv7 in its declaration and tombstone. Migration also records each
reassigned legacy bus identity with the subject that kept it and the archived
subject's new ID, so readers of legacy records never retype colliding bytes
into the wrong subject. Migration resolves supervisor
references against the combined pre-migration live-and-archived subject index
and rewrites every reference to the parent's migrated ID in the same
transaction. A missing or ambiguous reference refuses before writes and
requires pre-activation unarchive and repair before retry. A frozen legacy ID
becomes opaque and remains unchanged after a host move.

An ID survives routing, presentation, graph, host, desired-state, and
runtime-incarnation changes. Retirement makes a subject non-routable and
releases its address but preserves its ID. Reintroducing the same ID denotes
the same subject; a replacement receives a new generated ID.

Agent Spec adds an optional mutable **agent address**. The effective address is
the explicit value when present and otherwise the positional `identity` value.
It is unique per logical host among running and suspended subjects;
`<host>.<address>` is the **bus address** used for ordinary human routing.
Explicit addresses are bounded dotted lowercase alphanumeric/hyphenated
segments. The first explicit address atomically replaces the legacy fallback.
Every later address change is also an immediate cutover. Old addresses receive
no alias, redirect, history, or expiry period and may be reused.

Ordinary references resolve addresses. Exact ID lookup uses explicit syntax or
a typed API and never falls through to address lookup. Durable graph edges,
ownership, authorization, message provenance/replies, and default runtime task
identity use the immutable ID. Existing declaration-parent state and Resource
paths do not move. `ST_AGENT` carries the ID as an exact actor selector rather
than a mutable address.

`name` and `description` remain mutable, non-unique presentation. They never
route or identify a subject.

Address authoring uses the existing source-preserving, authority-scoped,
transactional catalog publication model. The complete prospective catalog
validates catalog-global ID uniqueness and host-local effective-address
uniqueness, including collisions between explicit addresses and identity
fallbacks. An address-only change preserves the live process and every durable
subject surface. Host, graph, and launch changes preserve the logical subject
ID but retain their field-specific runtime lifecycle.

This decision independently accepts the bounded presentation, constrained
authoring safety, and nondisruptive PTY projection proposed in draft decision
0002. It supersedes that draft's claim that positional identity and
host-qualified bus identity are the sole stable routing keys.

## Consequences

- `new-agent` can create a subject immediately with an opaque durable ID, omit
  or use a provisional address, then refine its route after the first prompts.
- Existing live and archived declarations need one additive `id` migration, and
  supervisor references become ID-keyed in the same transaction. A live subject
  keeps its former bus identity; only an archived collision receives UUIDv7 to
  preserve global uniqueness. Runtime and declaration-parent durable state
  require no re-key. An absent `address` preserves the current route until first
  assignment.
- A legacy ID may retain an obsolete host-looking prefix after a host move. Its
  type, not its string shape, distinguishes it from a current bus address.
- A host move does not change logical subject identity or default task identity,
  although the process incarnation can still change under the host lifecycle
  contract.
- Stale human routes fail loudly. The system does not accumulate compatibility
  aliases or make correctness depend on time.
- The implementation must change every identity-bearing boundary coherently;
  the open implementation delta prevents the target VRS from being mistaken for
  shipped behavior.

## Amendment 1 — 2026-09-05: the address ships first, the ID half is staged

The decision above stands as the target. Its two halves ship separately, because
only one of them answers the Context: the mutable address does, with no
migration, no record version, and no activation gate. Splitting a provisional
route from a durable key needs one new field; freezing an explicit ID buys
host-move invariance and live/archive collision attribution, both real and both
with no observed instance on any admitted host.

Shipped now: the optional `id` and `address` grammar with catalog-global `dup-id`
and host-local `dup-address` admission, `effective_id`/`effective_address`/
`bus_address`, roster and graph projection, `st2 agent address`, and the
fail-closed bare-or-qualified reference resolution — which every reference plane
now shares, including stream ingress. Positional `identity` remains the durable
key, so `ST_AGENT`, task IDs, session socket paths, declaration-parent state,
harness records, PTY tags, and supervisor edges are untouched by a cutover. A
`supervisor` value is a declaration key on both of its planes — the org-chart
walk and the notices that walk carries (crash-loop, protocol rejection) — so an
address is a routing alias for human and message references only, never a
supervisor reference.

Deferred: UUIDv7 creation, the `st2 catalog migrate-ids` freeze transaction,
ID-keyed durable records (message version 2, harness-state and harness-context
version 2, PTY schema 2), collision metadata and collision-aware attribution of
reassigned legacy endpoints, and the activation gate that would sequence them.
No part of it is contradicted by shipping the address first: `id` and `address`
are independent fields, and the freeze migration is unchanged by their order.

The ID half re-enters on any one of these observations, each of which turns a
latent argument into a live obligation:

- a completed cross-host seat move, or a concrete plan for one — the frozen ID is
  the only thing that preserves `ST_AGENT`, task IDs, and socket paths across it;
- a live/archive identity collision on any admitted host, which is what the
  reassignment record and legacy-endpoint attribution exist for; or
- a UUIDv7 creation call site — a generator that authors new subjects — because a
  subject whose ID is in no address namespace cannot be reached without ID-keyed
  resolution.

Two corrections to the premises this decision was implemented against:

- **Version-1 readers are additively tolerant, so a new field is not a version
  bump.** `crates/st2-wire/src/lib.rs` states the opposite of the assumption as
  policy: no type in the reader crate uses `deny_unknown_fields`, precisely so a
  reader older than the binary it shells out to ignores unknown fields. The one
  genuine cross-build hazard is single-field and about routing, not records: a
  build that does not read `address` routes the positional identity and refuses
  the new address, the exact inverse of a build that does. So the reader-first
  obligation is "deploy an `address`-reading build on every admitted host before
  authoring any address", and nothing more. A durable record that *does* reject
  unknown fields — `SentRecord` — keeps its version-1 shape here, so its version-2
  reader belongs with the writer that emits one.
- **Ownership keys are not addresses.** An exact selector answers to either
  immutable key: the explicit `id`, or the positional `<host>.<identity>` bus
  identity that migration would freeze into it. Both are unique by admission and
  neither moves when an address does, which is what keeps a running seat, its
  resync stream, and an interrupted send bound to their own subject across a
  cutover.

Before the ID half can land, two defects found while auditing its
implementation must be fixed:

- `supervisor_chain::resolve_spec` resolves parents by `bus_id(host)` or bare
  `identity` only, so it must accept `effective_id`; otherwise a subject born with
  UUIDv7 loses its org-chart edge, which is the clause the migration transaction
  exists to satisfy;
- `st2 catalog migrate-ids` must exempt `agent-id-missing` from its own
  pre-admission gate. A partially migrated catalog is inadmissible by that
  diagnostic, and the migration verb refuses to write into a non-admitting
  catalog, so the rollout order the decision prescribes — teach the projection to
  emit `id`, then migrate — deadlocks on the only command that can clear it.

## Options

| Option | Result | Reason |
| --- | --- | --- |
| Immutable ID + distinct mutable unique address + non-unique presentation | Selected | Matches the graph-subject and Git-branch-alias model and permits semantic route refinement without re-keying continuity. |
| Existing host-qualified bus identity as explicit legacy ID | Selected | Preserves current state and distinguishes valid same-identity subjects on different hosts; the original host prefix becomes opaque after migration. |
| PTY two-axis model: use non-unique `name` as the route | Rejected | It removes a field but does not provide the distinct mutable unique semantic address Johannes requires. |
| Keep exact ID and address in one precedence-based positional resolver | Rejected | Existing semantic IDs would keep old route spellings alive after cutover, contradicting immediate rename semantics. |
| Preserve old addresses as aliases | Rejected | Adds permanent reservation, removal, collision, and compatibility obligations without a stale-caller requirement. |
| Re-key legacy state to UUIDv7 | Rejected | Requires an unproved live migration of task/PTY, inbox, archive, context, provider, and external-reference state. |
| Require `address` in every declaration immediately | Rejected | Creates a cross-repository flag day and mixed-version deployment hazard; optional fallback is additive. |
| Current host-qualified placement as mutable ID | Rejected | Makes a host move change the logical graph subject identity; the selected legacy ID instead freezes the original bytes. |
| ULID or custom random token for new IDs | Rejected | UUIDv7 is standardized and the ID is not the ordinary human-facing route, so compactness does not justify a custom identity format. |

## Evidence and Argument

Evidence gathered on 2026-09-04 and 2026-09-05:

- issue #401 records the current overloading and a disposable-catalog continuity
  experiment;
- PTY PR #139 proves immutable-ID-first resolution, duplicate human labels, and
  fail-closed ambiguity diagnostics; PTY PR #142 proves atomic exact-ID metadata
  projection;
- st2 `message::resolve_agent_handle` already uses catalog-generation fencing;
- R27 catalog authoring already provides complete prospective validation,
  compare-and-swap publication, durable generations, and crash fencing;
- Johannes confirmed the graph-subject/branch-alias scenario and q1-q12, then
  selected frozen existing bus identities for colliding legacy IDs in q13.

Implementation acceptance requires model-free tests that prove: UUIDv7 creation,
explicit-ID migration for live and archived declarations, deterministic archived
collision handling, atomic supervisor reference migration, catalog-global ID
uniqueness across the live catalog and archive, optional-address parsing and
legacy fallback, host-local effective-address collisions, explicit ID versus
ordinary address selection, dotted-reference disambiguation including
host-pinned qualified input, immediate old-address failure and reuse, retirement
release, ID-safe unarchive, and safe reactivation; atomic cutover under
concurrent readers/writers, constrained self/supervisor authoring, and Nix
refusal; unchanged task/PTY PID, creation identity, generation, provider
session, inbox, archive, context, Resource state, declaration-parent state paths,
and existing long-form task IDs after address changes; retained subject ID with
existing lifecycle behavior for host/graph/launch controls; message-version
reader-first compatibility, including collision-aware attribution of legacy
endpoints whose bytes migration reassigned, and typed non-Agent endpoints; and
stable machine-readable roster, graph, task, message, and diagnostic shapes.
