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
- **T02 Native ownership over heuristic reach:** A managed harness that cannot
  declare a matching native session driver and non-secret delivery readiness is
  rejected rather than routed through generic terminal injection. Ding remains
  available only for opaque non-harness PTYs.

## Requirements

### Must implement the agent contract

- **R01 Agent-spec compliance:** st2 validates and implements every agent-spec
  capability it claims to support, and identifies unsupported capabilities.
- **R02 Canonical KDL and declaration identity:** Hand-authored KDL is the
  canonical declaration; any generator is optional and its output is
  inspectable before reconciliation. Declarations are discovered recursively.
  The target Agent Spec carries an explicit catalog-global `id`; positional
  `identity` and `host` remain authoritative declaration and legacy-address
  inputs independent of folder path. Either omitted legacy field retains its
  path-derived default and mismatch diagnostics. The declaration's parent
  remains its state/resource anchor. Dot-prefixed and other organizational
  lifecycle meaning; discovery excludes `.git` and `.st2` control directories
  at any depth, the catalog-root `pty` runtime directory, and state namespaces
  directly owned by a declaration.
- **R03 Host-pinned placement:** Every runnable agent or task resolves to its
  declared host; host-local roots own reconciliation.

### Must provide intelligent host supervision

- **R04 Root supervision:** Every machine has exactly one root agent. The
  deterministic st2 reconciler keeps declared local processes converged; the
  root observes host-local runtime health, diagnoses failures, performs bounded
  recovery, and escalates what it cannot resolve.
- **R40 Prompt catalog convergence:** A resident catalog supervisor begins a
  new serialized reconciliation pass promptly after a supported declaration
  publication becomes durably visible; it does not normally wait for the
  periodic full-audit interval. Transactional st2 writers and authorized direct
  atomic declaration replacement remain independent wake sources, so
  degradation of one source does not disable the other. Accepted events
  coalesce without concurrent passes. The periodic timer remains the
  correctness fallback for event loss, unavailable platform watchers, and
  unsupported direct filesystem mutation, and each degraded wake source emits
  an operator-visible diagnostic.
- **R31 Reachable restart bounds:** Within one supervisor run, restart
  accounting is per task and only successful launches spend the declared
  budget. `delay` is the minimum launch spacing in either mode. In delay mode,
  `attempts` limits launches over the sliding `interval` window without parking
  the task. In fail mode, `attempts` is reachable independent of reconcile
  cadence: a task that repeatedly launches and dies without being observed
  alive on every completed accounting pass for a full `interval` is parked for
  the remainder of its owning supervisor run after its successful-launch
  budget is exhausted, unless that run grants an explicit per-task unpark
  request. Granting the request clears exactly that task's park decision and
  restart accounting; it does not release another parked task or perturb a
  healthy task, including an identically named task owned by another supervisor
  scope. A completed pass that does not observe the task alive breaks accrued
  recovery uptime rather than forgiving failures through silence.
- **R32 Bounded helper teardown:** After st2 spawns a bounded non-interactive
  helper, input delivery failure or deadline expiry targets the helper's entire
  process group, not only its direct child, so teardown includes descendants
  that outlive that child. st2 either reaps the direct child before returning or
  transfers wait ownership to a background reaper; the failure remains bounded
  and reports its originating input error or timeout.
- **R34 Bounded helper output capture:** Capturing a spawned non-interactive
  helper's stdout/stderr consumes memory bounded by a fixed per-stream cap
  independent of the child's output volume and of how many captures run
  concurrently. When a stream exceeds the cap, the retained bytes are that
  stream's tail, and truncation is observable. A caller that must consume a
  stream whole (structured data for parsing) opts in through an explicitly
  named capture path, so an unbounded read is always visible at its call site.
- **R39 Disjoint harness-process lifetimes:** Every long-lived st2 process
  started by an integration/evaluation harness is classified at its spawn
  boundary as either a durable detached task or a parent-bound owned child
  group. A durable detached task survives normal return, unwinding, and hard
  death of the supervisor that launched it; only explicit lifecycle
  reconciliation may terminate it, and replacement adopts the same runtime
  identity. A parent-bound group is terminated on normal return, unwinding,
  and hard parent death. Its cleanup targets the exact owned generation rather
  than a reusable bare PID, has a finite synchronous deadline, transfers any
  unfinished waits to a reaper, and includes descendants plus every direct
  child that requires reaping. Cleanup of one lifetime class never crosses
  into the other.
- **R22 Quiet coordination after events:** A network with minimal or default
  personas stays quiet while useful work continues. Agents coordinate only after
  an inbox DING, a durable failure, a real blocker, a completion or decision
  handoff, or a declared schedule with a name. They continue until they resolve
  the need or hand it off. Repeated status messages and peer polling are not
  substitutes. Normal supervisors handle failures and blocked work. They do not
  continuously manage healthy work. A custom supervisor persona can require
  more frequent coordination. CoS is only an example. st2 does not define or
  require that role, and it gives the role no standard authority. Transport
  loss does not stop independent host-local work.

### Must preserve delivery and launch behavior

- **R05 DING/archive semantics:** Inbox delivery, archive precedence, retries,
  suppression, and restart recovery are deterministic and tested. DING may
  interrupt agent work, but it must not alter or submit a human's active draft;
  an unknown interaction state defers delivery.
- **R06 Restartable launch definitions:** A restarted PTY or exec receives the
  complete effective launch definition, including environment and supported
  launch fields.
- **R42 Launch argv transparency:** Every st2-added launch wrapper preserves the
  effective task program and each argument as ordered, opaque OS strings. A
  Linux systemd scope launch disables systemd command-line environment
  expansion before the wrapper separator, so dollar-bearing literals including
  `$HOME`, `${UNSET}`, and `$$` reach the task byte-for-byte. Detached and
  degraded-detached launches remain exact pass-throughs.
- **R07 Verified hooks:** Required hook content is installed explicitly and
  verified before a rendered agent depends on it. The selected receipt carries
  the binary's real source identity regardless of build system. Ordered
  upgrades are automatic; selecting an older, unorderable, or unreadable exact
  hook set requires explicit replacement authority. A selection change does not
  invalidate another running binary's previously installed immutable set. Hook
  interpreters and runtime dependencies are portable and explicit on every
  supported package environment.
- **R33 Provider-visible session restoration:** A shipped session-start hook
  restores fresh, non-empty durable context and its boot ritual through the
  provider's supported model-visible output contract. The complete context is
  preserved without truncation or an argument-size boundary. Missing or stale
  context is an ordinary cold start, and unavailable enrichment dependencies
  fail open without blocking provider startup. Any other construction or
  delivery failure is distinguishable from an ordinary cold start and follows
  R17's durable error-propagation contract.
- **R11 Control-plane replacement safety:** Stopping or killing `st2 up` must
  not stop, restart, or replace any agent it launched. st2 can be reinstalled
  and restarted while running agents continue unchanged; the replacement
  control plane adopts those existing processes by immutable agent ID and
  starts only genuinely missing work. Stopping an agent is a separate, explicit
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
  to run the world. st2 classifies source, path, kind, and affected agent ID or
  task identity, then takes the shortest correct path from observed state to
  desired state.
- **R14 Explicit filesystem-event contracts:** Every watcher is deny-by-default
  with exact roots, paths, mutation kinds, semantic meaning, debounce policy,
  and consumer. Reads, opens, unknown paths, and runtime output never trigger
  generic reconciliation.
- **R15 Bounded event coalescing:** Accepted event streams use tested
  head/tail coalescing: immediate head response, one quiet tail, and a hard
  maximum preventing starvation or unbounded scans, PTY queries, launches,
  delivery attempts, or writes.

- **R16 Supervisor declaration:** Every non-root agent declares exactly one
  supervisor by immutable agent ID; root is the only agent without a
  supervisor. Legacy supervisor references migrate to the resolved parent's
  migrated ID in the same catalog transition that adds explicit IDs.
- **R17 Durable error propagation:** Lifecycle, harness/eval, provider-turn,
  task/exec/PTY, hook, and delivery errors are durably reported to the
  responsible supervisor with agent ID, task identity, and actionable context.
- **R19 Targeted reconciliation and typed agent selection:** An exact
  agent/task selector resolves one catalog-global immutable agent ID and the
  subject's current pinned host before mutation. Unknown, ambiguous, and
  wrong-host targets refuse before writes, listing, or actions.
  Materialization, hook gates, PTY inspection, and plan execution are limited
  to the selected owner/task; unrelated diagnostics remain visible while
  unrelated workspaces, tasks, and live PTY PID/generation stay unchanged.
  Automation, ownership, authorization, and durable graph edges use the agent
  ID through an explicit ID selector or typed API. Every agent-selecting command
  keeps address and ID inputs disjoint. When a command defaults from
  `ST_AGENT`, it consumes that value as a typed exact ID and never reparses it
  as an address. An ordinary human reference may be a bare agent address or a
  host-qualified bus address and resolves only when the complete catalog proves
  one subject. Presentation values never resolve a message, Resource, status,
  lifecycle, or authoring target.
- **R20 Portable Resource bindings:** An agent may directly carry zero or more
  order-independent Resource bindings. Each binding has a non-empty, agent-local
  unique name and preserves an RFC 3986 absolute URI byte-for-byte without
  normalization. Its URI scheme is the exact lookup key for an optional,
  catalog-declared Resource Profile; scheme meaning remains downstream-owned,
  and st2 ships no built-in profiles. A registered profile may give the URI a
  contained local denotation under
  [`07-resource-profile`](07-resource-profile/requirements.md); an unregistered
  scheme stays opaque. The generic envelope itself does not imply resolution,
  access, readiness, or lifecycle semantics, and declarations that add such
  unsupported policy are rejected rather than silently ignored.
- **R21 Nondisruptive Resource observation:** Machine-readable catalog
  inspection exposes every Resource binding without interpreting its profile or URI.
  Resource-only declaration changes do not alter a task's effective launch
  definition and do not stop, replace, or relaunch healthy work.
- **R27 Transactional catalog authoring:** One st2 publication operation admits
  exactly one canonical KDL Agent Spec, with explicit ID, host, and identity,
  against the complete prospective catalog and structural archive ID index.
  Publication is compare-and-swap, durable, and atomic: readers observe either
  the previous declaration set or the next complete set. Reconciliation holds
  one coherent declaration snapshot
  through materialization, runtime observation, planning, and execution, so a
  retirement cannot commit and then be followed by a launch from stale catalog
  input.
  A durable incomplete-apply marker fences every declaration-plane snapshot and
  action after a crashed whole-catalog apply; a resident supervisor stays alive
  but performs zero lifecycle actions until the transaction is completed.
  Every successful declaration commit advances a durable monotonic catalog
  generation; whole-catalog apply advances it before its marker clears. Unlocked
  diagnostic readers therefore detect even a completed declaration ABA across
  their observation. A durable incomplete-generation intent fences readers
  across each single-writer commit and is conservatively recovered by the next
  exclusive writer. Existing-catalog writer staging exists only in the reserved
  control plane, never among authoritative declaration leaves; fresh bootstrap
  stages one non-authoritative sibling because its control plane does not exist
  yet.
  Presence, messages, context, and Resource state remain independently writable
  and are never serialized behind catalog authoring.
  A publication that replaces an incumbent declaration carrying an ownership
  marker is admitted only when the caller asserts exactly that marker, which
  makes publication no weaker an authority boundary than the lifecycle verb
  authoring the same bytes; an unasserted replacement refuses only for the Nix
  marker, and a mismatched, unresolvable, or malformed assertion always fails
  closed. Creating a declaration and republishing byte-identical bytes replace
  no owned bytes and require no assertion; a candidate that merely claims a
  marker is not an authority, so the receipt records only a marker the
  incumbent confirmed.
  A caller binds single-agent publication to the exact no-follow source capture
  with an authoritative input digest. A canonical whole-catalog snapshot
  externalizes the declaration-root digest while excluding runtime state and
  workspace content. Its closed projection includes every regular file in a
  bounded `_templates` library and exact declared canonical workspace directory
  facts. Whole-catalog apply binds its exact captured desired projection to a
  caller-supplied input digest before locking or mutation, rechecks the incumbent
  root digest under the exclusive lock, durably stages the desired bytes, and resumes
  after interruption solely from a closed marker and its content-addressed
  stage. A distinct raw-preimage projection may bind a repair or migration to
  the exact structural declaration bytes of any incumbent without interpreting
  those bytes. It has a separate hash and receipt type, admits only a fully
  valid prepared result, and persists its mode in the recovery marker. Raw mode
  makes no semantic assertion about the incumbent, including its validity or
  effective PTY root; the caller's exact raw-domain digest is the complete live
  precondition. Fresh-catalog bootstrap is a distinct create transaction,
  not a catalog-apply mode: it binds an exact captured prepared projection to a
  caller-supplied digest, initializes the persistent authoring lock and first
  catalog generation before visibility, and publishes the complete catalog by
  one durable no-replace directory rename. A retry is unchanged only when the
  completed existing catalog has the exact prepared declaration root. Bootstrap
  validates and preserves one explicit external PTY root but performs no PTY
  registry I/O; process adoption and PTY-root migration remain outside its
  atomic boundary because the registry has independent producers. Apply never
  traverses, hashes, deletes, or relocates workspace or runtime state. An absent
  canonical identity becomes
  visible only as a complete bundle; a preexisting declared workspace skeleton
  remains safe because the durable marker fences declaration readers and
  marker-time state routing throughout leaf publication and verification.
  A policy-free prepared-catalog comparison takes the existing shared lock,
  binds the live side to a caller-supplied declaration-root digest, captures the
  prepared side through retained no-follow capabilities, and fully admits both
  projections before returning one versioned receipt. The receipt exposes the
  before/after declaration roots, exact added/removed/modified projected paths
  with render/template/static classification, and normalized per-agent
  Agent Spec-model field-address changes. Addresses expose only the structural
  and dynamic address keys needed to locate a field; payload values and
  per-value/agent hashes remain private. Omission and explicit defaults are
  equivalent. Comparison never
  writes live declarations, generation/marker state, runtime/state/workspace
  bytes, or the prepared source, and carries no migration policy or publication
  authority.
- **R23 Fail-closed task inventory:** One read-only machine command exposes
  every desired local PTY and exec task by immutable agent ID, nullable current
  bus address, task name, runtime id, kind, lifecycle, retirement, desired
  state, runtime state, PID, creation time, opaque runtime-generation id, and a
  required resource-observation target. Runtime ID remains task identity. PID,
  creation time, generation ID, cgroup path, and process-tree root are
  observation locators only and may change between samples.
  The resource target is a closed tagged union. A proved live Linux generation
  exposes its exact unified cgroup-v2 path read from that process's
  `/proc/<pid>/cgroup`; unit and scope names are not locators. A proved live
  Darwin generation exposes its PID as a best-effort process-tree root. For
  PTY, st2 captures the candidate daemon's kernel start token, takes one
  socket-backed `pty stats --json` snapshot for all candidates, and admits the
  token only when the named live session reports the same daemon PID and
  creation generation and a second token read is unchanged. That token fences
  target observation, as the recorded token does for exec, but its transient
  availability never changes the PTY generation ID. A changed, recycled, or
  exited process is never associated with the target.
  Non-running, indeterminate, degraded, and unsupported observations expose a
  bounded explicit unavailable reason rather than a nullable or guessed locator.
  Resource-target unavailability is itself a complete observation and does not
  turn a trustworthy task inventory into an incomplete one.
  A proved non-routable retired subject has a null bus address without making
  coverage incomplete. Unknown, duplicate, malformed, unreadable, timed-out,
  PID-reused, or otherwise unprovable runtime evidence is indeterminate and
  makes the versioned envelope incomplete and the command unsuccessful; it is
  never reported as absence. The command projects a supervisor's known park
  fault and per-task recovery action alongside the unmodified runtime
  observation. The action is structured executable argv carrying the exact
  canonical catalog folder and selected host, so invoking it never falls back
  to ambient catalog or host defaults. A believable park remains complete,
  while an unbelievable park marker fails closed like other unprovable
  evidence.
  Observation detects semantic declaration drift across its runtime probe,
  does not invoke a backend for a root positively absent at admission, and
  performs no reconciliation, cleanup, lifecycle change, state rewrite,
  resource sampling, or persistent locator registration. An admitted PTY root
  that changes filesystem identity during the backend probe makes the
  observation incomplete; the external backend may already have recreated a
  concurrently removed registry. This diagnostic boundary is not
  transactionally serialized with catalog or runtime writers and is not
  control-plane cutover authority. It samples the durable catalog generation
  and incomplete marker around discovery and runtime observation; any marker,
  malformed fence, or catalog-generation change makes the envelope incomplete.
- **R24 Immutable agent ID and mutable agent address:** Each logical agent
  subject has one explicit, catalog-global, immutable agent ID. IDs are unique
  across the live catalog and structural archive, across hosts and desired
  states. The ID survives address, presentation, supervisor, graph-placement,
  host-placement, desired-state, archival, and runtime-incarnation changes. New
  subjects use UUIDv7. During migration, every live legacy subject receives its
  existing host-qualified bus identity as its explicit ID. A structurally
  archived legacy subject receives those same bytes when they remain unique
  across the combined live-and-archived subject set; an archived collision
  receives UUIDv7 and records it in its declaration and tombstone. Migration
  also durably records each reassigned legacy bus identity together with the
  subject that kept it and the archived subject's new ID, so readers of legacy
  records never retype colliding bytes into the wrong subject. This
  preserves current runtime and archived continuity without admitting duplicate
  IDs. Supervisor resolution uses the combined pre-migration live-and-archived
  subject index. The same transition rewrites every reference to the parent's
  migrated ID. A missing or ambiguous reference refuses before writes with
  `legacy-supervisor-unresolved`; the operator must unarchive and repair that
  declaration through the pre-activation legacy authoring path, then retry.
  A frozen legacy ID becomes opaque and keeps its original host-looking bytes
  after a later host move. Retirement makes the subject non-routable but
  preserves its ID. Reintroducing the same ID denotes the same subject; a
  replacement subject receives a newly generated ID. Suspended subjects retain
  their route and occupy the address namespace. Retired subjects release their
  address.
  An Agent Spec may declare an optional mutable `address`. Its effective
  address is the explicit value when present and otherwise the positional
  legacy `identity` value. An explicit address is at most 255 ASCII characters
  and is a dotted sequence of 1-to-63-character segments. Each segment contains
  only lowercase letters, digits, and hyphens and begins and ends with a letter
  or digit. Effective addresses are unique per logical host among running and
  suspended subjects. Host qualification produces the human-routable bus
  address `<host>.<address>`. Assigning the first explicit address atomically
  cuts ordinary routing over to it. The prior effective address stops resolving
  immediately and is reusable; st2 retains no implicit alias, redirect,
  history, or timed compatibility route. Exact ID lookup uses only explicit ID
  syntax or a typed API.
  Address-only changes preserve the logical subject, declaration-state anchor,
  ID-keyed supervisor edges, inbox, archive, context, Resource state, task IDs,
  live PTY PID/creation identity/generation, and provider session. A host, graph,
  or launch change retains the subject ID but follows its own runtime lifecycle
  rule.
- **R25 Bounded presentation and constrained identity authoring:** Agent Specs
  may declare optional, non-empty `name` and `description` strings in canonical
  KDL and the readable TOML/JSON forms. `name` is a non-unique mutable human
  label, limited to 160 Unicode scalars; `description` is an enduring
  responsibility boundary, limited to 1,000. Both are single-line: Cc control
  characters and U+2028/U+2029 are invalid. Omission means absence.
  Presentation is never an address or ID, and the declaration is its sole
  source of truth; a sibling `name` file is ignored without migration or
  compatibility behavior.
  `st2 agent address`, `st2 rename`, and `st2 describe` set or clear only their
  corresponding direct field in one canonical KDL declaration selected by
  explicit immutable ID. The address command validates host-local effective
  address uniqueness against the complete prospective catalog. All three
  preserve unrelated source bytes, serialize cooperating local writers through
  the persistent shared `.st2/catalog-authoring.lock`, reject a stale source
  before atomic replacement, fsync the result, and return classified receipts.
  The lock inode is never removed or stale-recovered and defines one local
  POSIX filesystem/kernel exclusion domain; it is not cross-host coordination
  or OS isolation from direct external writers. TOML, JSON, declarations
  explicitly marked Nix-owned, agent-ID changes, and malformed or ambiguous
  targets fail closed. Nix emitters must publish that marker before authoring
  is activated. In the trusted-fleet model, caller-supplied `ST_AGENT`
  provides a guardrail, not authentication: a catalog agent may edit itself or
  a descendant reached through ID-keyed supervisor edges, while its absence
  selects the operator path.
- **R26 Nondisruptive identity and presentation projection:** Runtime
  ownership, default task IDs, provider bindings, and `ST_AGENT` derive from
  immutable agent ID rather than mutable address or presentation. Compact agent
  lowering assigns its canonical task the explicit ID `<agent-id>`. Any
  long-form named task without an explicit task ID defaults to
  `<agent-id>.<task-name>`, including a task named `agent`; an explicitly
  authored task ID remains authoritative. State and Resource paths remain
  anchored at the declaration parent under R02. For every healthy managed PTY,
  st2 reconciles a versioned owned metadata snapshot containing immutable actor
  ID, current bus address, and optional description through one exact task-ID
  metadata patch. Only the canonical compact agent task whose task ID equals
  the agent ID maps optional name to native PTY display metadata; other PTYs
  preserve their task-specific display convention. Projection
  preserves unrelated tags, removes absent owned values, reports and retries
  failure, and is idempotent. Address and presentation projection never uses
  human-address resolution or enters launch, teardown, garbage collection,
  replacement, or flapping accounting. The current lowered Agent Spec remains
  the only source of address, name, and description; an explicit ID roster
  query returns the subject's current address and presentation without
  publishing a duplicate Agent Spec state file.
- **R41 Typed agent desired state:** Every admitted Agent Spec has exactly one
  whole-agent desired state: `running`, `suspended`, or `retired`. Omission and
  legacy `retired #false` mean running; legacy `retired #true` means retired
  without a rationale. New suspended and retired declarations require one
  valid human rationale of 1..160 UTF-8 bytes. Running forbids a rationale, and
  a declaration that mixes legacy `retired` with `desired-state` is invalid.
  Running uses ordinary task reconciliation. Suspended and retired agents do
  not launch or materialize tasks and tear down every live owned task,
  including generated companions. Suspension preserves the declaration,
  inbox, context, resources, and existing `keep` and `adopt-only` policy; resume
  grants no replacement authority beyond ordinary reconciliation. Retirement
  retains its stronger collection contract.
- **R28 Desired-state authoring and observation:** `st2 agent desired-state`
  changes lifecycle intent only in one canonical KDL declaration selected by
  explicit immutable agent ID. It uses the same source-preserving, durable,
  exact-target, trusted-fleet authority boundary as presentation authoring and
  refuses malformed, ambiguous, or unsupported declarations. A Nix-owned
  declaration refuses authoring unless the caller asserts exactly the ownership
  marker that declaration carries; a mismatched, absent, unresolvable, or
  malformed assertion fails closed, and the asserted arm additionally admits the
  complete prospective catalog exactly as a compare-and-swap publication of the
  same bytes would, so it cannot commit a declaration the catalog would reject.
  It is the only authoring verb with that authority, because a projection has
  one transition its own source cannot express: the source change being
  projected is the seat's removal. A
  transition from retired to running or suspended validates effective-address
  uniqueness against the complete prospective catalog before publication,
  including the positional identity fallback. Running is canonically omitted;
  suspended and retired states persist their rationale. Its receipt proves the
  declaration edit, never runtime convergence. Human listing, roster JSON, task
  inventory, and Doctor expose desired state without conflating it with
  presence or observed liveness.
- **R29 Visible, nondisruptive launch drift:** st2 derives a versioned desired
  launch fingerprint from the minimal effective fields it already passes to a
  task. Only an st2 launch records an observed fingerprint, bound to that
  launch's exact runtime identity and creation incarnation. A missing or
  mismatched binding reports `unknown`; otherwise inspection reports
  `converged` or `drifted`. Healthy `drifted` and `unknown` tasks are adopted
  without implicit restart.
- **R30 Explicit replacement boundary:** An absent or dead active task launches
  from the latest current declaration. Retirement stops live work and prevents
  relaunch. Replacing live work is a separate, explicitly selected operation
  fenced by catalog, host, effective PTY root, task set, and an exact runtime
  identity recheck immediately before disruption. Replacing an immutable agent
  ID remains retire-old/add-new; changing an agent address follows R24's
  nondisruptive cutover. Every behavior remains complete with an ordinary
  catalog folder and without CAS, captured generations, or replacement
  journals.
- **R35 Authoritative admitted graph:** `st2 catalog graph --json` is the sole
  catalog topology authority. For each uniquely admitted agent it publishes
  the effective native session driver plus direct parent, root, depth, and
  nearest-parent-first ancestor facts. Duplicate agent ID, missing parent,
  supervisor cycle, bounded-depth overflow, and a per-host root count other
  than exactly one are errors; affected topology facts are null and the graph
  is incomplete. Consumers do not reimplement those generic graph rules.
- **R36 Explicit native delivery readiness:** Managed Claude, Codex, pi,
  OpenCode, OMP sessions declare their matching native session driver and one
  tagged delivery-readiness value. Credential readiness may name a non-secret
  account identifier or leave selection to the driver. Anonymous OMP readiness
  names OMP and a non-empty normalized model set. Readiness is declaration
  state, never inferred from activity, argv, process names, or credentials.
- **R37 Retirement settles the inbox:** Every reconciliation of a retired
  local agent first tears down every live owned task. Only after every teardown
  attempt for that agent succeeds does the pass archive its canonical inbox
  messages; any teardown failure leaves the entire inbox in place and retries
  teardown plus settlement on the next reconciliation. The archive filename is
  the durable receipt: repeated settlement and a sync-restored duplicate remove
  the inbox copy without overwriting the archived bytes. Suspension retains the
  inbox and does not settle it.
- **R38 Codex schema admission:** Codex app-server launch is gated by an
  admitted fingerprint of the delivery-critical schema projection. Admission
  proves each emitted or consumed method discriminator is linked to its exact
  payload arm and recursively covers the referenced definitions. Behavioral
  turn, resume, and receipt evidence is reviewed separately; a schema match
  alone does not claim it.
