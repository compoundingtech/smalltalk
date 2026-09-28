# st architecture

st is the Small Talk claims-graph runtime. KDL declarations are its human authoring surface. Claims
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
10. The st2 and st control loops remain separate during migration.

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

`st launch preview` validates and renders a planner-authored launch candidate.
`st missions publish FILE --as ACTOR` previews and then atomically applies exact authored mission
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

A Claude harness can stop at a screen that no hook reports: an expired login or the workspace trust
prompt. The reconciler reads the terminal screen for both. Either one fences that incarnation as not
ready with its reason (`providerAuth` or `providerTrustPrompt`). A login needs a person. A trust
prompt does not: the reconciler stops that exact incarnation and starts a replacement, whose driver
admits the workspace again before Claude starts. Nothing is typed into the terminal. After three
trust prompts in ten minutes the reconciler stops replacing the seat and asks the operator.

The driver admits a workspace under Claude's own config lock (`.claude.json.lock`). Every Claude
process re-reads and replaces `.claude.json` under that lock, so no running Claude can publish an
older copy of the config that lacks the new workspace.

Native harnesses receive the same generated `.st3/boot.md`. A harness prompt can add stable
repository context. It cannot replace the runtime contract.

## Fault isolation

The reconciler takes up each item of a pass on its own. The items are:

- each member;
- each observer, schedule, and subscription;
- each mission run, and each step within it;
- each later stage of the pass: intake, observers, schedules, scheduled work, subscriptions,
  provider-capacity retries, retired-agent attention, mission evaluation, and attention `until`
  conditions.

When an item fails or panics, the reconciler records the fault on that item's subject and carries
on with every other item:

- a member fault is a `runtime.reconcile-decision` claim;
- an observer, schedule, subscription, mission-run, or step-run fault is a `reconcile.fault`
  claim on that subject;
- a stage fault is a `reconcile.fault` claim on `daemon/HOST`, naming the stage.

A fault is recorded again only when its cause changes. The item's next success records its
recovery.

Sometimes a fault cannot be recorded on its item, for example when a store write fails. The item
was still skipped on its own and the pass carried on. The daemon then records `daemon.diagnostic`
with the code `fault-record-failed` and status `faulted`, and the host is not reported as
unreachable.

A member declaration that this build cannot read is not skipped silently. The host that published it
records a member fault naming the parse error, until a build that can read it takes it up.

The reconciler reads each source of its next wake-up time on its own: mission deadlines, work wakes,
provider-capacity retries, and subscription retries. A source that fails records a stage fault
named `deadline/SOURCE` and is read again within five seconds, and the other sources keep their
deadlines. A step whose wake cannot be read loses only its own wake-up, with a `wake-deadline`
fault on its step-run.

A running member whose workspace or render fails is still observed, checked, and woken for its
work. The failure blocks only its start and restart.

Two members can render different bytes to the same file. Then only the member that would change
the file on disk faults. The member whose content is already there keeps rendering, so declaring a
new member never takes down one that runs.

Sometimes the PTY registry does not answer, or one PTY's record cannot be read. Terminal members then
wait for the next readable snapshot: none is started, restarted, or recorded as stopped. Exec
members and the rest of the pass still run.

A member whose start keeps failing does not spawn again on every pass:

- it waits 15 seconds between attempts;
- after three failures within five minutes it parks with one attention request.

Cleanup of a cancelled, failed, or finished run reads only the declarations that the run owns. It
never waits for the run's mission revision or its steps, so a run whose revision is unavailable
still stops its runtimes.

Cleanup waits at most 15 minutes for the run's runtimes to report stopped. A runtime on a host that
never answers, or one that cannot be killed, would otherwise hold the run and its active-run slot
forever. At the deadline the run ends, with a reason naming each runtime still live. Their stop
declarations stay, so stopping continues after the run ends.

A panic that escapes a pass restarts the reconciler with backoff and records `daemon.diagnostic`
with the code `reconciler-panicked`.

The daemon's other loops keep the same rule:

- The local API keeps serving when one accept fails, for example when the daemon runs out of file
  descriptors.
- A native delivery forwards each message on its own. A message it cannot forward, such as one
  whose document is not on this host, is recorded once as a `harness.diagnostic` with the code
  `message-unforwarded`. The recipient's other messages keep arriving.
- A driver skips renewing a step whose claim ended in the meantime, and keeps running.
- A panic while the store's writer is held does not disable the store. The panic rolls back its
  open transaction as it unwinds, and the next write proceeds. The reconciler's own locks recover
  the same way.
- A wake message whose close fails does not keep an agent's other messages open or delay its next
  wake.
- Session discovery skips a transcript it cannot read and lists the rest.
- Every `pty` command and the `git ls-files` check that render makes have a time limit. The
  reconciler runs them inline for every member, so one command that stops answering cannot stall
  the host.

## Messages and attention

Small Talk messages are durable claims. Delivery is a separate lifecycle with sent, delivered,
read, and closed facts.

The reconciler creates ready-work messages. A driver transports them and renews claimed work. This
split lets a ready step survive a daemon outage, a driver outage, and a failed delivery.

An agent notification only indicates that ready work or a message may exist. It does not authorize
new work. The work queue and message record remain authoritative.

`st attention ls --as person/NAME` combines that person's current human gates, launch approvals,
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

One st node is a complete local system. Fleet nodes exchange authenticated claim envelopes and
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

Each member’s render transaction refuses symbolic-link escapes and conflicting file ownership.
It refuses to replace a tracked `.st3/boot.md` with different bytes. Additive Git excludes combine
across operations and worktrees sharing an exclude file. A failed member pass records a fault
without blocking other members; stopping a member does not require its render to succeed.

Mission constraints describe required outcomes. They do not disable harness features or replace a
future operating-system sandbox.

## Migration boundary

The st2 catalog remains a searchable archive. st does not import st2 inboxes, archived messages,
runtime records, conversation history, or durable context.

Move one agent at a time. Stop the st2 owner before starting an st mission that uses the same
workspace. Keep the old declaration for rollback until the st run passes its checks.

See [agent migration](agent-migration.md) for the complete sequence.
