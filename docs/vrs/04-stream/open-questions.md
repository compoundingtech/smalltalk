# Stream open questions

Each entry links a spec `DQ-S*`. Questions leave this file when resolved —
into [spec.md](./spec.md) as decisions or [`.experiments/`](./.experiments/)
as tested hypotheses.

- **DQ-S1 Producer reply routing.** Events use `<host>.<agent>/<stream>` as
  their producer identity, but that subordinate identity does not currently
  resolve as an agent or service-principal mailbox for an ordinary
  `message reply`. Resolves by: specifying a real reply recipient with an
  end-to-end proof. Typed request retirement remains blocked until the
  eval-owned external requester has that working path.
- **DQ-S3 Ring bound and identity horizon.** `K = 128` is the implemented
  deduplication and conflicting-content-detection horizon, not merely a fast
  path: an evicted identity is accepted as new, without scanning inbox or
  archive history. That is no longer a divergence: ratified STREAM-R04/R05 and
  decision 0004 both say the retained receipt ring is the idempotency boundary
  and that an archive receipt keeps its authority for a known filename without
  being an event-identity index. What stays open is only the *value*: `128` is
  unmeasured. Resolves by: measuring real adapter emit rates and
  retry/rediscovery windows (CI transitions, builds, timer sources), then
  keeping the bound, raising it, or choosing a different bounded index.
- **DQ-S4 Request absorption staging.** The typed request/reply envelopes
  (`request.rs`) are absorbed by events + ordinary replies (decision 0004),
  but its wire types carry `deny_unknown_fields` and its invariant row names
  live tests. Resolves by: a staged plan — land replacement proofs including
  the routable reply endpoint from DQ-S1, re-point the invariant row, retire
  the module behind a deprecation window; blocked until both the stream
  implementation and that reply path are merged.
- **DQ-S5 Top-level shared streams.** One adapter feeding many agents
  (STREAM-T02). Must be defined as a generalization: a nested stream is a
  top-level stream whose owner and sole recipient is the enclosing agent.
  Blocked on: real demand — two catalogs actually duplicating an adapter.
- **DQ-S6 Parked-stream owner notification.** A crash-looping stream parks and
  surfaces to the declared supervisor; the owning agent — whose eyes just
  closed — learns nothing. Should the owner also receive one event/notice?
  Resolves by: operating v1; if agents act on stale world-views because a
  stream died silently, the answer is yes.
- **DQ-S7 Event body bounds.** Issue #238 wants bounded inbox bodies for
  one-inference DING handling; the DING frame carries no body, so an event's
  subject is the entire wake-time signal. st2 enforces a 1,000-byte subject
  ceiling but currently no body ceiling. Resolves with #238's outcome; until
  then adapters keep subjects self-sufficient and impose a small explicit body
  bound rather than attaching unbounded provider/build logs.
- **DQ-S8 One-shot stream completion.** The task model has no run-to-completion
  lifecycle (`TaskLifecycle` is `Service | AdoptOnly`), so a wait-adapter that
  exits after its terminal event relaunches, flaps, and parks — a fault report
  for a successful wait. The accepted doctrine (spec: "Waits are standing
  feeds") makes this the rare residual case: common waits ride standing keyed
  feeds like `pty-lifecycle-watch`. If completion semantics are ever needed,
  they belong in the task model (`TaskLifecycle`/`JobType`, adjacent to root
  DQ1's scheduled exec work), not as a stream-level flag. Blocked on: real
  demand — an agent whose one-off custom waits are frequent enough that
  add/rm plus a kept-alive adapter measurably hurts.
