# st2 vision

## The Problem

1. **Agent specs need an executable eval runtime.** A spec cannot be proven
   through prose; evals need a runner that executes it reproducibly.
2. **Proven agent specs need a real-work runtime.** The same declarations must
   run day-to-day agent work, or eval evidence and production behavior will
   drift apart.
3. **That runtime must be reliable in operation.** Launch configuration, hooks,
   messages, and durable state can drift or disappear across restarts and
   machines. Without externalized state, humans and supervisors must inspect
   terminals to learn whether an agent is present or making progress, while
   failures still need intelligent diagnosis and recovery.

## The Vision

- A reliable, declared runtime for agent specs.
- Per-machine schedulers that together form a distributed workflow engine for
  agent work.
- One intelligent root agent on each machine to supervise local runtime health,
  recovery, and escalation.
- Catalog-visible agent presence, activity status, and current plan progress.
- Durable message passing and encapsulated agent state.
- A concrete implementation of the executable agent spec maintained by
  `compoundingtech/evals`.

## What This Is Not

- A general messaging or identity platform for people, services, or arbitrary
  non-agent actors while the agent and st2 specs are still stabilizing.

## Success Criteria

1. Every supported agent-spec capability has an implemented and validated st2
   path, with unsupported behavior stated plainly.
2. A canonical agent declaration can be validated, materialized, launched, and
   restarted without losing its effective launch definition or durable state.
   The st2 control plane itself can be stopped, killed, reinstalled, and
   restarted without stopping or replacing any running agent; the replacement
   runtime adopts the existing agent processes without duplication.
3. Each supervisor tree has one root agent that observes its runtime health,
   resolves bounded failures, and escalates what it cannot resolve.
4. A human, CoS, root, or supervisor can inspect catalog-backed state to
   determine an agent's presence, activity status, current plan, and current
   plan step without opening its PTY or relying on its transcript; stale state
   is explicit.
5. Host placement, hooks, message delivery, archive handling, and recovery have
   explicit contracts and executable evidence.
6. Agent work can be scheduled or triggered as durable, inspectable workflows
   across the declared fleet.
7. Agent messages arrive reliably without corrupting, combining with, or
   submitting a human's active input.
