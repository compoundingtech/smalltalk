# Stream requirements

## Context

This subsystem defines declared event streams: named agent-owned subscriptions
that turn external-world changes into durable, deduplicated inbox events,
waking the agent through the existing delivery transports. It refines root
requirement [`R05`](../requirements.md) for inbox delivery, [`R22`](../requirements.md)
for named declared subscriptions ("do not replace the schedule with repeated
messages or polls" — a stream is the declared name a poll hides behind), and
inherits immutable agent ID and mutable address semantics from `R19` and `R24`.
Ordinary messages remain owned by
[`03-message/requirements.md`](../03-message/requirements.md); terminal
delivery remains owned by [`01-ding/requirements.md`](../01-ding/requirements.md).
The record-kind and locality decisions are recorded in
[`.decisions/0004`](../.decisions/0004-stream-events-are-a-distinct-record-kind.md)
and [`.decisions/0005`](../.decisions/0005-streams-are-agent-nested-and-stream-named.md);
executable evidence lives in [`.experiments/`](./.experiments/).

## Assumptions

- **STREAM-A01 Host-local streams:** A stream runs on its owning agent's
  declared host (`R03`). Cross-host observation is served by placing the
  adapter on the right host, not by remote streams. Ingress executes only in
  that owner's local lock domain: the declaration's resolved logical host must
  match a machine-local catalog/host binding established by the active
  supervisor. The binding is not synchronized and records the local catalog
  lock identity; caller-controlled logical `--host` cannot create it. A
  synchronized remote catalog copy is not publication authority.
- **STREAM-A02 Trusted producers:** Emitting into a declared stream is gated
  by the declaration's existence and the trusted-fleet model (root `A02`), not
  by authentication. An external producer that names an undeclared stream is
  refused.
- **STREAM-A03 World logic stays outside:** Adapters (GitHub pollers, timers,
  monitoring bridges) are host tooling packaged outside st2; st2 carries no
  HTTP client, scheduler, or provider-specific event logic.

## Acceptable Tradeoffs

- **STREAM-T01 No catch-up:** A restarted or resumed stream has no cursor; it
  re-observes current state and leans on event dedup. Missed intermediate
  transitions are not reconstructed.
- **STREAM-T02 Per-subscriber adapters:** Two agents watching the same feed
  run two adapter processes. A shared top-level stream form is future work and
  must be a generalization of the nested form, not a second mechanism.
- **STREAM-T03 No declared cadence:** v1 ships no `every`; a periodic source
  is a long-running adapter that sleeps and emits. Declared cadence, if it
  ever arrives, extends restart policy rather than adding an in-process loop.

## Requirements

### Must be a declared subscription

- **STREAM-R01 Declared streams:** An agent declares zero or more named
  streams. A stream exists iff declared. A stream with exactly one adapter
  form — `command` or `argv` — lowers to one derived exec companion (the
  stream task) that supervises the adapter; only a stream with neither form is
  an external ingress endpoint. Stream names follow the task-name grammar and
  are unique per agent alongside task names.
- **STREAM-R02 Self-authoring:** An agent adds or removes its own streams by
  editing its own declaration through the serialized catalog-authoring path,
  under the same authority boundary as presentation authoring (`R25`). Add
  exposes mutually exclusive shell `--command <shell>` and direct non-empty
  `-- <program> [<arg>...]` argv forms, or neither for external ingress, while
  preserving unrelated source bytes and the exact authored launch values. To
  remove an adapter-bearing stream, the serialized path identifies its exact
  derived runtime, stops/retires it, and durably confirms process absence
  before publishing declaration removal. Failure before that source publish
  leaves the declaration intact; adapter-less external ingress has no runtime
  teardown. Direct manual KDL edits are outside this guarantee.

### Must ingest exactly once

- **STREAM-R03 Mandatory event identity:** Every emit names the target agent,
  a declared stream, and a producer-supplied `event-id`. Emits without an
  `event-id` are refused. Dedup scope is `(stream, event-id)` per recipient.
  Admission and publication linearize while holding the catalog-authoring lock
  across strict declaration/desired-state revalidation and the stream-state
  transaction; lock order is catalog-authoring then stream state. This
  linearization exists only in the recipient owner's host-local lock domain;
  remote callers are refused before publication.
- **STREAM-R04 Idempotent ingress:** One external event becomes exactly one
  durable inbox record while its identity remains in the stream's retained
  receipt ring. Replaying an `event-id` within that horizon — including
  concurrent and crash-interrupted retries — returns the original filename,
  creates no second record, and never re-notifies only when content, key,
  supersession intent, and the originally selected predecessor semantics all
  match. Reuse with changed intent is a conflict, not deduplication. Once
  evicted, the identity is honestly accepted as new; an archive receipt
  retains its existing authority for a known filename but is not an
  event-identity index. The machine receipt distinguishes `created` from
  `deduplicated`.
- **STREAM-R05 Bounded stream state:** Per-stream durable dedup state is
  constant-size: a bounded receipt ring plus one in-flight publication
  reservation, including at most one selected predecessor receipt. The ring
  defines the deduplication,
  conflicting-content-detection, and supersession-lookup horizon. Event
  publication does not write the Agent Sent ledger and performs
  history-independent work per emit: it never scans inbox or archive history
  to recover an evicted event identity or predecessor. A later emit
  reconciles an abandoned reservation by validating its chosen file and
  completing its stored compaction intent against the predecessor's retained
  receipt. Absence from the inbox proves completion only through an
  authenticated same-name archive receipt; absence from both fails closed.
  Recovery does not require the producer to replay stale state and never
  archives unvalidated bytes.

### Must deliver as ordinary inbox work

- **STREAM-R06 Ordinary transport:** An event is an ordinary inbox record
  carrying event frontmatter (producing stream, `event-id`, optional `key`).
  Delivery rides the unchanged inbox paths; DING renders events with the `»`
  marker and inherits every fail-closed guarantee untouched.
- **STREAM-R07 Producer-side supersession:** An emit may declare supersession:
  the successor publishes before the newest still-unread matching predecessor
  among the stream's retained receipts is archived (`key` scopes the match;
  absent `key`, the whole stream does). Still-unread requires an inbox file and
  no same-name archive receipt. Immediately before either the initial or a
  recovered archive move, the predecessor's no-follow regular bytes and
  parsed event identity must match its retained receipt, and the move must be
  bound to that validated inode rather than re-resolving a replaceable
  pathname. Any inability to prove or move that exact object fails closed
  while preserving the inbox wake. A crash between those steps may leave both
  events unread but never removes the only wakeup; the selected predecessor
  filename and identity are retained in both pending and final receipts so
  recovery and replay preserve that exact compaction semantics. Supersession
  never touches DING staged ownership — a staged notice is released only
  through the existing archive-receipt rule — and a fast-superseding stream
  stays within `R15` bounds.

### Must couple to the owning agent's lifecycle

- **STREAM-R08 Companion lifecycle:** The stream task launches with its agent
  and is stopped while the agent is held, suspended, retired, or terminally
  parked, under the existing derived-companion contract. A crash-looping
  adapter parks the stream task without disturbing its agent and surfaces to
  the declared supervisor. Public `stream rm` holds catalog and exact-runtime
  lifecycle serialization across stop confirmation and declaration commit, so
  no reconcile pass can relaunch the adapter in between and no removed stream
  leaves an orphan process.
- **STREAM-R09 Suspension means eyes closed:** While an agent is suspended no
  events accumulate for it. Resume re-observes current state; re-emitting
  still-current state is safe under `STREAM-R03` dedup. Suspension and stream
  removal serialize with owner-local emit through the catalog-authoring lock:
  whichever owns it first is the linearized operation, and no emit admitted
  after the lifecycle/catalog change can publish under stale eligibility.
  This guarantee is scoped to the owner's local lock domain; cross-host
  producers must forward to an owner-host adapter/transport before ingress.

## Evidence

Four executable records in [`.experiments/`](./.experiments/): the ingress
comparison, the lifecycle spike, and the two design-pole explorations
(unification vs differentiation), each with the tests it ran and the
pre-existing failure set it verified against baseline.
