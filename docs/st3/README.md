# st documentation

The root [README](../../README.md) explains the product and the normal command workflow.

Use these documents for implementation details:

- [Architecture](design.md) defines the stable system shape.
- [Mission graph runtime](mission-graph-runtime.md) defines the KDL language and execution model.
- [KDL lifecycle](kdl-lifecycle.md) defines day-to-day publication and revision workflows.
- [Owned sets](owned-sets.md) defines complete membership publication, source ordering and retirement.
- [Seat rollout](owned-seat-cutover.md) defines idle cutover, strict native-session continuity and source status.
- [Free mode](kdl-lifecycle.md#free-mode): within a fleet, an agent may do anything the person who
  runs the fleet may do, as itself, until principals and grants land.
- [Schema registry](schema.md) lists the generated public subject, resource, and claim vocabulary.
- [Data authority](data-authority.md) separates durable facts from projections and caches.
- [Claim backups](backups.md) explains live snapshots, offline restore, and recovered writer identities.
- [Fleet replication](replication.md) defines convergence, inspection, and repair.
- [Agent seat queues](seat-queue.md) explains how each seat orders its mission runs and how moves
  are recorded and replicated.
- [Checkpoints and trimming](checkpoints.md) explains how every node of a fleet agrees to delete old
  claims together: when a checkpoint is due, seal, verify and trim, which claims a rule may drop and
  why, excusing an unreachable member, and what the first real trim taught.
- [Lanes](lanes.md) defines the ordered lanes a mission run works through, such as the merge
  train, and `st lanes`.
- [Attachments](attachments.md) defines the images a message carries between machines: where the
  bytes live (never in sync), the limits, upload and read, and delivery to a seat.
- [Resource subscriptions](resource-subscriptions.md) defines observers and automatic intake.
- [Suspending a seat](suspend.md) explains `st agents suspend` and `resume`: when a seat is quiet
  enough to stop, and how each harness comes back on its own native session.
- [Seats across deploys](seat-deploys.md) explains how a running seat's driver and channels follow
  a replaced st binary without ending the provider session, and how st reports a stale message path.
- [Delivery probes](delivery-probes.md) describes token-free native-channel probes, per-direction
  read latency, overdue attention, and the replicated results in `st doctor`.
- [Live-path priority](priority.md) explains how the daemon and each PTY server outrank the builds
  and tests their harnesses run, on Linux and macOS, and what needs root. It also explains how
  stopping a seat ends every process the seat started, and what a host without systemd misses.
- [Subagents](subagents.md) explains how st records the subagents a seat's harness runs as claims
  on the seat, with a lease, and ends them when their harness, session or seat goes away.
- [Model accounts](accounts.md) explains how a person declares several Claude and Codex accounts, how a
  seat binds one or a pool, and how a pooled seat at its limit restarts on another account.
- [Command recorder](command-recorder.md) explains how every `git` and `gh` call st starts is
  recorded, the log format, and what the recorder cannot see.
- [Profiling the daemon](profiling.md) explains how the daemon accounts for its own time: waits
  for the writer and read connections, SQLite statements, CPU, and callers.
- [Operational queries](operational-queries.md) explains bounded mission lists, outcome history,
  mission run summaries, and the five-minute performance report in `st doctor`.
- [Agent migration](agent-migration.md) defines isolated rehearsal, cutover, and rollback.
- [Eval audit](eval-audit.md) records the test intent and prompt boundary for each st eval.
- [Running st with omp](omp.md) covers omp seat setup, behavior, and known limits.
- [Product roadmap](roadmap.md) records accepted future work.
- [Guided CLI tour](cli-guided-tour.md) is the complete human walkthrough for every public command
  and subcommand.
- [TUI and Expo iOS design-session brief](app-design-session-brief.md) captures the product promise,
  delivery gates, remote transport research, and autonomous release loop to review before UI work.
- [Client v0 contract](client-v0/README.md) defines the shared TUI and mobile JSON, event, action,
  pairing, and terminal protocols.
- [Operational-state contract](operational-state/README.md) separates immutable history, current
  projection, and actor-specific actionable views and defines screen/CLI parity.

The [examples](../../examples/st3/README.md) show small, tested mission patterns. Use the evals for
failure proof, not as introductory examples.

## Observe, don't instruct

st observes agents and does not instruct them. It learns what an agent does from driver hooks and
harness events: sessions, turns, plan mode, subagents, tool calls, usage, and what a seat is blocked
on. It never asks an agent to report on itself and never tells an agent how to behave. A seat starts
idle with no prompt and takes no turn until a person types or a message is posted. Work reaches it
as a message that names a ready step, and the step's goals and constraints say what the work is.
st's only other text for agents is the skill that `st skill` prints, which describes how to use st
and sets no rules of conduct. Keep this rule when changing drivers, messages, or the skill.
