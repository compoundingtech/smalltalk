# st3 architecture

st3 is the Small Talk claims-graph runtime. KDL declarations are its human authoring surface. Claims
are its durable fact surface. Projections, process observations, and caches are derived state.

This document defines the stable system shape. The linked technical documents define the complete
interfaces and edge cases.

## Principles

1. A graph declaration describes desired state. It does not contain an imperative controller.
2. Publishing is an atomic upsert. Omission is not deletion or cancellation.
3. A mission run pins one immutable mission revision and one immutable input set.
4. Agents claim eligible work. The reconciler does not assign work by preference.
5. A run owns the lifetime of its declared agents, exec processes, and terminal processes.
6. Daemon failure does not terminate owned runtimes. A replacement daemon adopts exact survivors.
7. Invalid data remains visible. It cannot halt unrelated reads, writes, or replication.
8. Every repair adds a replacement fact and retains the invalid source for inspection.
9. Host placement is explicit. Cross-host replication is optional.
10. The st2 and st3 control loops remain separate during migration.

## Durable graph

The SQLite claims store contains immutable claim envelopes, documents, mission revisions, mission
runs, run generations, work leases, messages, reviews, and repairs.

Each subject has one deterministic current projection. Concurrent valid claims converge through
defined precedence. An unresolved or invalid head makes only its affected subject indeterminate.

See [data authority](data-authority.md) for the authority classes. See [schema](schema.md) for the
public subject and claim vocabulary.

## KDL publication

Every document starts with `version 2`. Declarations follow that node directly. There is no wrapper
that represents a database transaction.

`st3 launch preview` validates and renders a planner-authored launch candidate.
`st3 missions publish FILE --as ACTOR` previews and then atomically applies exact authored mission
KDL. Agents must already hold matching `mission-authority { publish ... }`; a declaration cannot
self-grant that authority. A failed declaration rejects the complete publication.

Removing a prior declaration from a later file has no effect. A cancellation, stop, repair, or new
desired declaration must state the change.

See the [KDL lifecycle](kdl-lifecycle.md) for the complete operator workflow.

## Missions and work

A mission defines goals, constraints, inputs, steps, dependencies, gates, products, members, final
work, and completion.

The default mission permits one nonterminal run. `concurrent-runs` permits independent overlap. A
mission without explicit completion uses the finite `all-steps-exhausted` default. Long-lived
harnesses are top-level agent seats rather than standing missions.

`assigned-to` names one eligible agent. Repeated `available-to` entries define a worker pool. A step
without a selector is agentless unless it inherits a selector from its containing graph.

A worker claims a ready step through an incarnation-bound lease. Completion is a worker report, not
a correctness result. Declared products and gates still control the graph transition.

Nested missions keep their own run identity and inherit their parent worker when the declaration
does not select another agent. A revision creates a successor generation. Compatible completed work
remains complete.

See the [mission graph runtime](mission-graph-runtime.md) for the complete language and state model.

## Runtime ownership

The mission run origin materializes its members. Another replica can inspect their claims but does
not start a duplicate runtime.

Every started member records its desired declaration, runtime identity, process identity, and
incarnation. Stop and adoption operations compare the exact incarnation before they act.

Status keeps process facts in `actual`. It keeps the current native harness observation in
`harness`. The harness view accepts only the current runtime incarnation or a legacy observation
recorded inside that runtime epoch.

A daemon restart observes the PTY and exec registries. It adopts a matching survivor and starts only
a missing desired member. Final work and explicit cancellation stop owned members.

Native harnesses receive the same generated `.st3/boot.md`. A harness prompt can add stable
repository context. It cannot replace the runtime contract.

## Messages and attention

Small Talk messages are durable claims. Delivery is a separate lifecycle with sent, delivered,
read, and closed facts.

The reconciler creates ready-work messages. A driver transports them and renews claimed work. This
split lets a ready step survive a daemon outage, a driver outage, and a failed delivery.

An agent notification only indicates that ready work or a message may exist. It does not authorize
new work. The work queue and message record remain authoritative.

`st3 attention ls --as person/NAME` combines that person's current human gates, launch approvals,
revision approvals, unread messages, and explicit fault requests. Human identity is required rather
than inferred. The stable client-v0 attention resource is the machine source for user interfaces.

## Resources and observers

A resource represents an external thing. An observer runs a bounded provider operation and publishes
typed current facts. An unchanged observation does not append a duplicate claim.

A subscription can start one exact mission revision when selected resource fields change. Each
delivery pins the exact triggering resource version.

Observers are independent runtimes. Their failure does not stop the daemon or unrelated graph work.

See [resource subscriptions](resource-subscriptions.md) for provider and intake contracts.

## Replication

One st3 node is a complete local system. Fleet nodes exchange authenticated claim envelopes and
document bytes over loopback endpoints carried by Fabric or an equivalent transport.

Replication compares immutable envelope inventories. Receipt order cannot change the deterministic
winner. Invalid envelopes remain inspectable and do not halt later replication.

See [fleet replication](replication.md) for configuration, convergence, diagnostics, and repair.

## Launches

A launch is graph state. It stores the exact request document, planner, candidate mission,
feedback, preview, and human decision. Its internal durable claims use the `planning-session`
subject family; clients and operators use only the launch noun.

Launch approval publishes a mission revision. It does not start the mission. A targeted launch
session can also propose a new generation for one active run.

The session can pause on one machine and continue on another after replication.

## Security boundary

The local API uses a Unix socket. Peer HTTP listeners and peer URLs must use loopback addresses.
Fleet messages use a shared secret and request-bound signatures.

The render transaction refuses symbolic-link escapes and conflicting ownership. It refuses to
replace a tracked `.st3/boot.md` with different bytes.

Mission constraints describe required outcomes. They do not disable harness features or replace a
future operating-system sandbox.

## Migration boundary

The st2 catalog remains a searchable archive. st3 does not import st2 inboxes, archived messages,
runtime records, conversation history, or durable context.

Move one agent at a time. Stop the st2 owner before starting an st3 mission that uses the same
workspace. Keep the old declaration for rollback until the st3 run passes its checks.

See [agent migration](agent-migration.md) for the complete sequence.
