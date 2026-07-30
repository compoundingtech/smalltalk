# st2 requirements

## Context

st2 implements the executable agent contract in
[`compoundingtech/evals/AGENT-SPEC.md`](https://github.com/compoundingtech/evals/blob/main/AGENT-SPEC.md).
The vision is defined in [vision.md](./vision.md). These requirements are the
current ratified product floor, not the entire future scheduler roadmap; new
scheduling and workflow constraints belong here only after their behavior is
accepted.

## Assumptions

- **A01 Declared fleet:** Each runnable agent has one current authored
  declaration and one pinned host.
- **A02 Trusted private fleet:** The catalog, runtime state, and participating
  hosts belong to one trusted operator; st2 is not an adversarial multi-tenant
  boundary.
- **A03 Durable host state:** Each host preserves its catalog and runtime state
  across process restarts. Whole-disk loss and backup are outside st2.
- **A04 Eventual transport:** Hosts may disconnect; Fabric eventually resumes
  transport when connectivity returns. st2 does not guarantee network
  availability.

## Acceptable Tradeoffs

- **T01 Explicit limits:** A documented unsupported case is preferable to a
  hidden distributed guarantee.

## Requirements

### Must implement the agent contract

- **R01 Agent-spec compliance:** st2 validates and implements every agent-spec
  capability it claims to support, and identifies unsupported capabilities.
- **R02 Canonical KDL:** Hand-authored KDL is the canonical declaration; any
  generator is optional and its output is inspectable before reconciliation.
- **R03 Host-pinned placement:** Every runnable agent or task resolves to its
  declared host; host-local roots own reconciliation.

### Must provide intelligent host supervision

- **R04 Root supervision:** Every machine has exactly one root agent. The
  deterministic st2 reconciler keeps declared local processes converged; the
  root observes host-local runtime health, diagnoses failures, performs bounded
  recovery, and escalates what it cannot resolve.

### Must preserve delivery and launch behavior

- **R05 DING/archive semantics:** Inbox delivery, archive precedence, retries,
  suppression, and restart recovery are deterministic and tested. DING may
  interrupt agent work, but it must not alter or submit a human's active draft;
  an unknown interaction state defers delivery.
- **R06 Restartable launch definitions:** A restarted PTY or exec receives the
  complete effective launch definition, including environment and supported
  launch fields.
- **R07 Verified hooks:** Required hook content is installed explicitly and
  verified before a rendered agent depends on it. The selected receipt carries
  the binary's real source identity regardless of build system. Ordered
  upgrades are automatic; selecting an older, unorderable, or unreadable exact
  hook set requires explicit replacement authority. A selection change does not
  invalidate another running binary's previously installed immutable set. Hook
  interpreters and runtime dependencies are portable and explicit on every
  supported package environment.
- **R11 Control-plane replacement safety:** Stopping or killing `st2 up` must
  not stop, restart, or replace any agent it launched. st2 can be reinstalled
  and restarted while running agents continue unchanged; the replacement
  control plane adopts those existing processes by stable identity and starts
  only genuinely missing work. Stopping an agent is a separate, explicit
  lifecycle action.

### Must externalize agent state and scope

- **R08 Catalog observability:** Catalog-backed state exposes each agent's
  presence, declared activity status, current plan, and current plan step
  without PTY or transcript inspection. Presence and activity status are
  distinct, and stale state is identifiable.
- **R09 State continuity:** An agent's current work and durable decisions
  can survive process replacement without depending on its transcript.
- **R10 Agent-only identity:** st2 models agents. Non-agent identities are
  unsupported.

- **R13 Shortest-path reconciliation:** An event is evidence, not permission
  to run the world. st2 classifies source, path, kind, and affected identity,
  then takes the shortest correct path from observed state to desired state.
- **R14 Explicit filesystem-event contracts:** Every watcher is deny-by-default
  with exact roots, paths, mutation kinds, semantic meaning, debounce policy,
  and consumer. Reads, opens, unknown paths, and runtime output never trigger
  generic reconciliation.
- **R15 Bounded event coalescing:** Accepted event streams use tested
  head/tail coalescing: immediate head response, one quiet tail, and a hard
  maximum preventing starvation or unbounded scans, PTY queries, launches,
  delivery attempts, or writes.

- **R16 Supervisor declaration:** Every non-root agent declares exactly one
  supervisor; root is the only agent without a supervisor.
- **R17 Durable error propagation:** Lifecycle, harness/eval, provider-turn,
  task/exec/PTY, hook, and delivery errors are durably reported to the
  responsible supervisor with agent/task identity and actionable context.
- **R19 Targeted reconciliation:** An exact agent/task selector resolves its
  identity and pinned host before mutation; unknown, ambiguous, and wrong-host
  targets refuse before writes, listing, or actions. Materialization, hook
  gates, PTY inspection, and plan execution are limited to the selected
  owner/task; unrelated diagnostics remain visible while unrelated workspaces,
  tasks, and live PTY PID/generation stay unchanged.
- **R22 Visible, nondisruptive launch drift:** st2 derives a versioned desired
  launch fingerprint from the minimal effective fields it already passes to a
  task. Only an st2 launch records an observed fingerprint, bound to that
  launch's exact runtime identity and creation incarnation. A missing or
  mismatched binding reports `unknown`; otherwise inspection reports
  `converged` or `drifted`. Healthy `drifted` and `unknown` tasks are adopted
  without implicit restart.
- **R23 Explicit replacement boundary:** An absent or dead active task launches
  from the latest current declaration. Retirement stops live work and prevents
  relaunch. Replacing live work is a separate, explicitly selected operation
  fenced by catalog, host, effective PTY root, task set, and an exact runtime
  identity recheck immediately before disruption. Renaming remains
  retire-old/add-new. Every behavior remains complete with an ordinary catalog
  folder and without CAS, captured generations, or replacement journals.
