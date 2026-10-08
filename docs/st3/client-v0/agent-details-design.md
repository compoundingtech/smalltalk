# Proposal: one selected agent details collection

Status: **design proposal, not an implemented client-v0 contract**. This document
proposes an additive read model for a web client. Existing schemas, fixtures,
HTTP operations and [collection subscriptions](collections.md) remain normative.
No new endpoint, observer, claim schema or generated client is introduced here.

## Problem and evidence

A details pane currently assembles independently fenced reads. Watching the
`agents` collection requires holding a fleet window: its status filter does not
select one agent. Other sections have no collection subscription at all.

A read-only measurement on daemon commit `2d440e7` on 2026-10-08 discovered 66
agents and sampled two agents, one working and one idle. Each route had ten
samples per agent, with globally serialized request starts at least three
seconds apart. All 180 sampled responses were HTTP 200. The following numbers
are HTTP request times, not WebSocket first-frame times or subscription CPU
costs; p95 is the maximum of ten samples, a coarse tail estimate rather than a
stable production percentile.

| Existing read | p50, working / idle | p95, working / idle | Complete response bytes, working / idle |
| --- | --- | --- | --- |
| Agent snapshot, including usage/context | 484.51 / 168.09 ms | 729.26 / 384.02 ms | 5,387–5,398 / 5,498 |
| Applied definition, environment values redacted | 128.68 / 52.15 ms | 2,068.51 / 1,163.22 ms | 2,632 / 2,340 |
| Declaration alternative | 15.93 / 14.05 ms | 173.53 / 40.97 ms | 3,375 / 2,882 |
| Observed resources, exact opener | 35.35 / 7.08 ms | 260.89 / 50.44 ms | 25,553 / 785 |
| Declared workspace | 52.91 / 22.89 ms | 850.85 / 76.35 ms | 682 / 658 |
| Terminal owner inventory | 16.34 / 34.77 ms | 93.75 / 317.34 ms | 1,239 / 1,177 |
| Agent queue | 1,105.42 / 696.17 ms | 1,852.66 / 3,183.24 ms | 438 / 424 |
| Person-wide attention, not agent-filtered | 387.29 / 387.06 ms | 1,693.78 / 743.23 ms | 36,384–39,958 in both slots |
| Status history | 389.74 / 402.89 ms | 832.94 / 856.09 ms | 17,595 / 20,928 |

Both queues were empty: no runs, moves or current work. Nonempty-queue cost was
not measured. Resources contained 18 and one items respectively; a separate
`limit=10` traversal returned 10 then eight items and `has_more:false`. Each
agent had one terminal; terminal attachment and screens were not exercised.
Both embedded subagent lists were empty, and both `checkout` fields were null.
The histories contained 96 transitions with `complete:true` and 125 with
`complete:false`. All twenty final attention items fit one person-wide page.
No latency SLO was supplied, so these are interactive concerns, not evidence of
a contract violation. This proposal makes no performance-improvement claim.

Transport/scoping and subscription availability below are **source-derived**,
not live paired-client or WebSocket measurements. The measurement predates the
source revision used for these references; line numbers in the measurement are
not assumed to match newer source. Relevant entry points are:

- [Agent detail and selected card construction](../../../crates/st3/src/api.rs)
  (`client_agents_detail`, `client_agent_resources_from_status`,
  `client_agent_cards_for_page`, `overlay_subagents`).
- [Client reads, collection selectors and scope dispatch](../../../crates/st3/src/api/client_v0.rs)
  (`CollectionSubscribe`, `subject_definition`, `agent_declaration`,
  `agent_workspace`, `terminals`, `agent_queue`, `status_history`, `authenticate`).
- [Resource paging](../../../crates/st3/src/api/client_v0/resources.rs),
  [resource projection](../../../crates/st3/src/store/resources.rs), and the
  [existing resource/workspace contract](README.md#resource-observations).
- [Roster dependency mapping and queue reducers](../../../crates/st3/src/store.rs)
  (`changed_agent_resources`, `seat_queue`, `seat_queue_inputs_tx`),
  [incremental collection delivery](../../../crates/st3/src/api/client_v0/collection_ivm.rs)
  and [shared window admission](../../../crates/st3/src/api/client_v0/collection_windows.rs).

## Selected row and one snapshot fence

Proposed collection name: `agent-details`. A subscribe command requires one
**exact, canonical agent ID**, not a prefix, status filter or fleet selector:

```json
{"kind":"subscribe","id":"details","collection":"agent-details","agent":"agent/example","person":"person/example","limit":1}
```

The collection contains zero or one `AgentDetails` row. The row ID is the agent
ID, `order` is empty or that single ID, and outer `has_more` is always false.
An unknown ID starts empty; retiring/removing the current agent removes the
row rather than silently turning it into historical detail. Historical agent
inspection remains a separate read. Missing a managed declaration is not the
same as missing an observed agent: such a row can have unavailable bindings.
`person` selects whose attention may appear; it is never inferred from an
agent's host, account or declaration. Other collection selectors are refused.

All nine sections are mandatory keys: `agent`, `bindings`, `resources`,
`workspace`, `terminals`, `subagents`, `queue`, `attention`, `status_history`.
Their data and freshness metadata are joined inside **one SQLite reader
snapshot** at store index S, with one captured evaluation time T. The outer
snapshot fence certifies the complete row; sections cannot perform independent
HTTP reads, select a later store cut, or splice in a previous result after a
failed read. A changed section sends a full-row upsert so the client replaces
all sections atomically. A commit during construction belongs to a subsequent
row, not the in-flight row.

The fence means current **admitted/projected store knowledge at S**, not an
atomic observation of every filesystem, process or remote service. A remote
observation must be admitted before S and retains its observation time and
origin/incarnation identity. No filesystem, Git, provider or network I/O occurs
while holding this SQLite snapshot. The owning daemon observes external state
and publishes it before the row is read; the connected gateway never probes
its own filesystem to answer for another owner.

Current agent cards also use local observation overlays and clock-sensitive
selection. Reusing their JSON without their dependency fences is insufficient.
A proposed details source must make every participating local receipt visible
in the same transaction, or certify a stable local-generation vector together
with S and T. A changed vector causes a restart/refusal of the read, not a torn
join. If such coverage cannot be certified, the affected observation is
explicitly `not_current`; it must not be promoted by a store-index-only cache.

## Freshness is a wire contract

Each section uses an explicit wrapper. The following is proposed wire shape,
not an example of a presently accepted response:

```json
{
  "freshness": {
    "state": "not_current",
    "basis": "observation",
    "source_index": 123,
    "observed_at": "2026-10-08T08:00:00Z",
    "valid_until": null,
    "reason": "observation_only"
  },
  "value": null
}
```

- `state` is `current`, `not_current`, `unavailable` or `forbidden`.
  `basis` is `declaration`, `projection`, `observation` or `history`.
  `source_index` is the last relevant admitted source index, no greater than S;
  it is not a substitute for the outer fence. A multi-source section records
  its dependency coverage in the source certificate, not a fabricated single
  claim ID. `observed_at` and `valid_until` are nullable timestamps.
- `current` certifies the named basis at S/T with complete invalidation and
  authority coverage. A current declaration means configured state, never
  evidence that a directory exists or a process obeys it. A current history
  means the retained history is up to date, not that it is complete forever.
- `not_current` may include useful last-observed data and its provenance, but
  must state why: for example `observation_only`, `source_gap`, `owner_unreachable`,
  `lease_expired`, `watch_lost`, or `refresh_pending`. Recent timestamps alone
  never make an external observation current. `valid_until` is an upper bound,
  not permission to label an untracked external fact current until a TTL.
- `unavailable` has no usable value and a reason such as `not_observed`,
  `unsupported_observer`, `ambiguous_declaration`, `read_failed`, or
  `payload_too_large`. An empty page with `current` means a certified empty
  result; an unavailable section must not masquerade as that empty result.
- `forbidden` returns no data, source IDs, counts or cursors from the denied
  section. It is distinct from a transient observation failure.

Mixed sections carry field/group-level freshness as well as the section
wrapper. For example, declared workspace can be current while observed dirty
state is not current; cumulative usage can be a current store projection while
context occupancy is only a last observation. The section's overall state is
`not_current` whenever an included group is not current. Actionability never
follows from freshness alone: all actions retain existing operational and
incarnation fences.

A known invalidation or source/authority coverage loss must first send an
additive, capability-gated `invalidated` control frame naming the subscription,
agent ID, affected sections and reason, before any coalesced rebuild. The client
marks the last row `not_current` until a full authoritative upsert arrives.
This frame does not pretend to be newly fenced data. It also invalidates section
continuations. A read failure sends the existing retryable `resync` plus the
section freshness state; it must not re-emit old JSON as current under a new
fence. Authorization revocation ends the subscription and clears denied data.
If control/data cannot be delivered within the existing send deadline, close
the socket rather than leave an apparently healthy current pane.

The source arms exact deadline wakeups for work/subagent leases, observation
coverage expiry and attention eligibility. No claim need arrive at expiry.
Clients invalidate at a supplied deadline even if no frame arrives, using the
server evaluation time to handle clock offset conservatively. Socket loss or
reconnect immediately marks the held row not current; reconnect's snapshot
replaces it. This is not periodic HTTP polling, and the existing thirty-second
clock refresh is not sufficient proof of currentness for this collection.

## Section sources, invalidation and bounds

The bounds below are proposed defaults and hard ceilings, subject to maintainer
review. Every section follows the freshness rules above, including failure and
oversize cases. Arrays are whole typed rows, never partially serialized objects.

### 1. Agent snapshot, usage and context

**Source.** Reuse the selected agent card reducer, not a fleet-window read.
`desired`, canonical `claims` and runtime/harness projections supply lifecycle,
reachability, declaration revision, runtime incarnation, current session,
operational reasons and existing queue previews. `usage_summaries_at` reads
`harness.usage` at S (response rollups, session cumulative usage and context
occupancy). Context includes model, used/window tokens, percent, compactions
and observation time. Local receipts must obey the joint fence above. See
[`api.rs`](../../../crates/st3/src/api.rs) (`client_agent_resources_from_status`)
and [`store.rs`](../../../crates/st3/src/store.rs) (`usage_summaries_at`).

**Invalidation.** Reuse the roster mapping: every registered claim on this
agent invalidates its reduction, including `intent.desired`, `runtime.observed`,
`harness.observed`, `harness.diagnostic`, `harness.usage`, `harness.timeline`,
`harness.todo.observed`, reconcile/rollout claims and account bindings.
Message lifecycle changes (`message.sent`, `.staged`, `.delivered`, `.read`,
`.closed`) map through projected sender/recipient, including ancestor-dependent
rollout blockers. Work/run/owner changes use the queue mapping below. Receipt
updates and the earliest observation/lease deadline also invalidate.

**Bounds and currentness.** One agent summary, one usage summary and one context
summary, up to 64 KiB combined; no transcript or unbounded usage series. Do not
duplicate growing queues or subagent arrays here: previews remain at most five
IDs and full bounded sections follow. Usage is current as a stored aggregate,
not a promise that all provider activity has already been reported. Context and
harness/process observations expose their own observation coverage and time;
without certified ongoing coverage they are `not_current/observation_only`.
A new incarnation invalidates old context even without a replacement sample.

### 2. Normalized declared bindings

**Source.** Derive from the selected applied `intent.desired` claim/`desired`
row and `MemberSpec`, using the same canonical AST reconstruction/redaction as
`subject_definition`. Preserve current desired token, supplying declaration
token and conflict state. Follow only unambiguous stop predecessors as
`agent_workspace` does. Normalize these named groups:

- `host`: configured host reference, not reachability;
- `workspace`: configured root and launch cwd, plus declared checkout
  repository/base/branch when present;
- `harness`: driver, model/effort and typed non-secret settings, not runtime
  readiness;
- `account`: configured references; distinguish declaration from a later
  effective `agent.account` observation/binding rather than overwriting one
  with the other;
- `terminal`: declared terminal mode and `bind-terminal` reference, distinct
  from observed terminal inventory;
- `authority`: explicitly declared mission/queue/seat/agent authority
  relationships, not a synthesized caller grant;
- `env_names`: sorted variable names only by default. Values are opt-in only
  with the existing additional `read.declarations` scope and redaction policy.
  Never expose the resolved process environment or credential contents.

See [`model.rs`](../../../crates/st3/src/model.rs) (`MemberSpec`),
[`graph.rs`](../../../crates/st3/src/graph.rs) (`render_agent_desired_kdl`,
`redact_agent_env_values`), and
[`client_v0.rs`](../../../crates/st3/src/api/client_v0.rs)
(`subject_definition`, `agent_declaration`, `agent_workspace`). The existing
`agent-declarations` route requires declaration scope even with values redacted;
projection-only details use the applied-definition policy, not that route's
historical AST/revision-list policy.

**Invalidation.** `intent.desired` on the agent, predecessor selection and
conflict resolution; changes to any referenced declaration used to resolve a
binding; `agent.account` for the separately labelled effective binding.
Pairing/delegation changes revalidate value access. Keep reverse edges for old
and new bindings so moving away from a reference invalidates its dependents.

**Bounds and currentness.** At most 200 normalized entries across all groups,
64 KiB including optional values. No KDL, raw AST or all-revisions list in this
row. Never truncate a declaration silently: an oversized group is
`unavailable/payload_too_large`, with authorized inspection through the existing
definition/declaration read. Conflicts are unavailable with conflict metadata,
not an arbitrarily chosen binding. A valid declaration is `current/declaration`
at S even if its target is absent; observed/effective bindings retain their
separate projection or observation basis.

### 3. Observed resources opened by the agent

**Source.** `resource_observations` supplies latest canonical observation facts,
using the existing `(opened_by, subject)` index and **exact**
`opened_by == agent_id`. This is not a list of every mentioned resource or every
resource opened by any run the agent touched. Optional `kind` and literal
`subject_prefix` filters remain conjunctive. See
[`store/resources.rs`](../../../crates/st3/src/store/resources.rs) and
[`api/client_v0/resources.rs`](../../../crates/st3/src/api/client_v0/resources.rs).
Attribution receipts may preserve first-opener identity for supported kinds;
reuse the existing merge reducer rather than reinterpret raw claims.

**Invalidation.** `resource.observed` on matching or previously matching
subjects, including attribution-only updates, fact replacement, loss/change
of `opened_by`, deletion, canonical record repair and replication source-cut
changes. A reverse opener index must consider both old and new membership.
This dependency must be added: roster-only classification is not coverage for
resource inventories.

**Bounds and currentness.** First 25 rows by subject, at most 200 per page and
128 KiB per section. Carry `has_more`, bound cursor and explicit count coverage;
never report a page count as a total. The inventory can be current as the
latest-observation projection at S; each resource's external facts carry
`observed_at` and `not_current/observation_only` unless its observer certifies
ongoing coverage. This section therefore cannot claim all external resources
are presently unchanged just because its SQLite index is current. Paging is
defined below; no full resource history is implied.

### 4. Workspace and observed worktree

**Source today.** `agent_workspace` returns declaration and host, not directory
existence or actual cwd/Git state. Snapshot `checkout` is configured metadata.
The owning reconciler's `observe_agent_workspace` currently writes
`workspace.observed` with host/workspace and an optional repository root;
[`reconcile.rs`](../../../crates/st3/src/reconcile.rs) contains the producer,
[`store.rs`](../../../crates/st3/src/store.rs) (`workspace_observation_from_host`)
the canonical origin-qualified lookup, and
[`st3-schema/src/lib.rs`](../../../crates/st3-schema/src/lib.rs)
(`workspace.observed`) the existing fields. None supplies dirty/branch/head or
a process's live cwd. Do not infer those fields from a declared checkout.

**Proposed observation boundary.** Extend an owner-side workspace observer,
not the HTTP handler. It observes on the agent's configured owner:
(1) runtime/harness cwd for the selected incarnation, with the observed process
identity; (2) filesystem root/worktree identity; (3) Git index/worktree dirty
state including tracked staged/unstaged and untracked files; (4) actual symbolic
branch, nullable when detached; and (5) actual HEAD object ID, nullable for an
unborn repository. Launch cwd is not live cwd; shell/job cwd is not assumed to
be the harness process cwd. Failed process inspection and non-Git workspaces
have explicit field reasons, never `dirty:false` or a guessed branch.

The owner publishes an admitted observation with `observed_at`, origin,
declaration token, runtime incarnation, cwd source, repository/worktree identity
and observation generation. Optional extension fields or a versioned observation
kind require separate schema review; no claim extension exists in this PR.
A filesystem watcher must cover index, refs, HEAD, worktree files and untracked
creation/removal, including linked worktree/common Git directories. Cwd needs
runtime/harness change events with equivalent coverage. If a bounded probe
cannot establish continuous coverage, publish its useful data as
`not_current/observation_only`, even immediately after the probe. No polling loop
or TTL-based claim of freshness is acceptable. Unsupported or inaccessible
observations are unavailable, not substituted with configured values.

**Invalidation.** `intent.desired`, `runtime.observed`, `workspace.observed`
(or its approved successor), runtime incarnation/placement changes and
owner-side cwd/filesystem/Git events. Watch overflow, observer restart,
repository replacement and owner loss revoke coverage immediately. Extend the
collection mapping: `collection_ignores` currently ignores
`workspace.observed`, which would be incorrect for details.

**Bounds and currentness.** One declared workspace plus one observed worktree,
16 KiB; no diff, directory walk listing or file names. Declared group is
`current/declaration`. Observed cwd/dirty/branch/head each carry freshness and
observation provenance. `current/observation` requires certified event coverage
through the admitted observation generation; otherwise they are explicitly
not current. Changing declaration/incarnation fences out the old observation.
An unreachable owner cannot be replaced by a gateway-local probe.

### 5. Terminal owner inventory

**Source.** Reuse `terminal_resource_status_at(owner, S, history=false)` and
`runtime_resources_from_status`, retaining exact `owner_id == agent_id`,
runtime ID, terminal ID, state, owner identity, incarnation and existing access
summary. Standalone terminals owned by the agent follow the existing ownership
rules, not a terminal-ID prefix guessed by the client. See
[`client_v0.rs`](../../../crates/st3/src/api/client_v0.rs)
(`terminal_resources_for_owner`) and
[`st3-schema/src/owned_terminals.rs`](../../../crates/st3-schema/src/owned_terminals.rs).

**Invalidation.** Canonical `runtime.observed` and `intent.desired` on the
agent and its owned terminal subjects; ownership, placement and owner-run
liveness changes; runtime observation coverage/deadlines and pairing scope
changes. Preserve old ownership reverse edges during transfer or retirement.
A screen update does not change this inventory unless its inventory facts
also change.

**Bounds and currentness.** Default 25 rows, at most 200 per page, 64 KiB;
owner/terminal ID ordering and continuation. Inventory is a current projection
only when runtime-source coverage holds; unreachable/expired runtime
observations explicitly mark affected rows not current. No screen bytes,
scrollback, attach capability or terminal sequence is joined here. Screens
still require `terminal.attach`, `terminal.read`, the attachment capability and
incarnation, then their own subscription slot. Inventory timing says nothing
about screen cost.

### 6. Harness subagents

**Source.** Reuse canonical claims on the parent:
`subagent.appeared`, `subagent.renewed`, `subagent.ended`, as reduced in
[`store/subagents.rs`](../../../crates/st3/src/store/subagents.rs) and joined by
`overlay_subagents` in [`api.rs`](../../../crates/st3/src/api.rs). Read at S/T,
including parent incarnation/session and lease. IDs, type, description, driver,
session/work IDs, started time and lease expiry are present-day harness
subagents, not the separately declared `under` relationships and not historical
child activity.

**Invalidation.** These three claim kinds on the parent, parent
`runtime.observed`, relevant harness end/restart, `intent.desired` stop/removal,
and the earliest live subagent lease expiry. Expiry can change membership
without a new claim; schedule an exact wakeup/removal. Do not reuse the old
array until another unrelated claim or periodic refresh happens.

**Bounds and currentness.** Default 25 rows, at most 200 per page, 64 KiB;
order by start time then subagent ID. An admitted live lease certifies the
lease-backed projection at T, not proof that a child process is presently
running. Parent observation loss makes the observational part not current;
expired entries leave the current page. Descriptions are bounded whole
fields; oversized individual entries are explicitly unavailable, not clipped.
No subagent action or terminal stream is invented by this read model.

### 7. Queue, work and missions

**Source.** One exact-agent queue reduction inside the same S/T snapshot;
reuse `seat_queue_inputs_tx` and canonical queue selection, joining
`step_runs`, `mission_runs`, `run_generations`, `mission_definitions` and
`desired`. `agent.queue.moved` supplies ordering/move provenance. Return
current work, next ready work, ordered run summaries with joined mission
ID/title/state, and bounded work summaries with path, title, first goal, state,
assignment and lease. Carry complete-count coverage separately from previews.
A mission summary is deduplicated by ID, and work/run detail is paged rather
than embedding every mission's historical steps. See
[`store.rs`](../../../crates/st3/src/store.rs) (`seat_queue`,
`seat_queue_inputs_tx`, `agent_work_queues`) and
[`client_v0.rs`](../../../crates/st3/src/api/client_v0.rs) (`agent_queue_value`).
The actor-filtered `work` collection is not a substitute for all queued work.

**Invalidation.** Reuse `changed_agent_resources`: `agent.queue.moved`;
`mission-run.created/state`; `run-generation.created/state/superseded`;
`intent.desired` on an agent/run/generation; `step-run.state/retried/carried`;
`work.claimed/renewed/progress/submitted/failed/released/extended` and
`work.person-asked/done/cancelled`. Map step assignee, lease owner, activity
actor and previous/current owning run/generation/declaration to affected
agents. Add mission definition/title/revision changes and dependencies used
by readiness/selection, including gates, loops and run wait relationships.
Use old and new assignment/run edges, not just current assignment, and
invalidate at the earliest work-lease/readiness deadline.

**Bounds and currentness.** At most 25 run summaries initially (200 per page),
25 work summaries initially (200 per page), five current/next/upcoming preview
IDs, 20 recent moves, and 192 KiB for this section. Each run's claimed/ready/
waiting work IDs is bounded with continuation, not an unbounded nested array.
Current-work overflow is explicitly paged; it is not hidden behind the preview.
Mission summaries are limited to the returned runs and share their page budget.
A queue is `current/projection` only with complete dependency/lease coverage.
A partial or failed queue source is not an empty queue. Work execution
observations retain their own freshness and action fences. This seam must not
call the existing expensive HTTP queue route once per subscription or scan all
historical fleet work to produce one agent's bounded page.

### 8. Agent-filtered attention

**Source and matching rule.** First obtain all authorized current attention
candidates for the explicitly selected person, using the same episode and
run-liveness reducers as the existing attention read; then filter **before**
ranking/limiting. Never filter a person-wide first page client-side or server-side
and assume the remainder contains no matches. For agent A define, at S/T:

- W(A): current operational work assigned to A or held by A under a live lease,
  plus the current origin work referenced by an authorized person ask whose
  requester is A. All associations are read from projected source relations,
  never recovered from titles, prose or ID prefixes.
- R(A): current runs in A's canonical queue, A's current owner run, and runs
  containing W(A). Membership is independent of the returned queue/work page.

An authorized current attention item matches if **any** of:
(1) its `targets` includes A exactly;
(2) `requester_id == A`;
(3) its `step_run_id`, origin-step relation, or a work target belongs to W(A);
(4) its `mission_run_id` or a run target belongs to R(A).
Return `matched_by` with the applicable structured reasons/IDs. Exact IDs are
compared after canonical parsing. A mission-only target is not enough: sharing
a mission definition is not sharing a run. No transitive ancestor/descendant or
fleet-wide match is implied. A shared run can intentionally yield the same
attention card in more than one agent's details. Losing association removes
the row even if the person-wide item stays open. Authorization always happens
before correlation; relations cannot reveal denied attention metadata.

Source references: [`store.rs`](../../../crates/st3/src/store.rs)
(`attention_snapshot`, `current_attention`),
[`attention_snapshot.rs`](../../../crates/st3/src/store/attention_snapshot.rs)
(`person_ask_context`, episode/liveness reducers), and
[`attention_ivm.rs`](../../../crates/st3/src/store/attention_ivm.rs)
(`local_attention_open`, `local_attention_open_dependencies`). The existing
keyed operators cover person-step and custom families, **not all attention**.
Reuse their reverse dependencies where certified; do not treat a partial
family index as a complete attention source. Legacy `attention.requested`/
`attention.resolved` audit claims are not the whole current-attention model.
Agent-owned faults in `fault_snapshot` are distinct from this person's
attention and are not silently unioned in.

**Invalidation.** Person asks (`work.person-asked/done/cancelled`), step state,
assignment/lease and W/R membership; mission-run/generation liveness;
`intent.desired` requester/ownership changes; gate requests/results and review,
launch/planning/revision decisions consumed by `mission_run_attention_items`;
`harness.observed/diagnostic` and account/login relations for login prompts;
registered custom-source claims and document/binding changes. Changes to any
target's current state invalidate its joined state. Add exact clock deadlines
for eligibility/grace rules and episode validity, plus pairing/person authority
changes. Follow complete canonical family dependencies, including source
corrections and deletion, rather than an unguarded claim-prefix allow-list.

**Bounds and currentness.** Default 25 cards, at most 200 per page, 128 KiB;
existing priority/newest/source/episode ordering. Return current episodes only,
not a historic request log. Without certified coverage of every participating
family and W/R relation, this section is `not_current/source_gap` or unavailable,
not an apparently complete list from the indexed subset. Bounded fallback reads
may serve a newly certified S/T snapshot; they may not reuse older cards as
current. The attention section is forbidden if the chosen person is not allowed.

### 9. Bounded status history with continuation

**Source.** Preserve the semantics of
[`store/seat_status.rs`](../../../crates/st3/src/store/seat_status.rs): transitions
of observed harness state and incarnation resets from `runtime.observed`,
`harness.observed` and relevant `harness.diagnostic` claims; suppressed
heartbeats are not new transitions. Fields remain state, observation time,
runtime incarnation and reset. These are not necessarily canonical agent
lifecycle transitions and are not an unbounded audit trail.

**Invalidation.** Those kinds on this agent, canonical source correction,
checkpoint retention/cut changes and the seven-day boundary. A heartbeat may
change current freshness even if it adds no transition. Reuse the reducer's
`history_source`/transition classification rather than a separate state machine.

**Bounds and currentness.** Default newest 50 entries, at most 200 per page,
64 KiB. Retain the existing seven-day / 200-transition maximum. Return
`has_more` and an opaque older-page cursor **within that retained range**, plus
`retained_from`, `retention_days:7`, `retention_max_transitions:200`, and
`complete` using the existing reducer's completeness meaning. `has_more:false`
does not imply `complete:true`; data outside retention is unavailable, not a
fictional continuation. Stable newest-first order uses observation time and
canonical source position as the tie breaker. A covered history is
`current/history` at S/T even when incomplete; source gaps are explicitly not
current. Add continuation without changing old `/status-history/{a}` responses.

## Incremental source and subscription cost

Reuse the roster incremental classifier and selected-card construction as the
shared agent/work dependency base, not a second competing allow-list.
`changed_agent_resources` in [`store.rs`](../../../crates/st3/src/store.rs)
invalidates all registered agent-subject claims, maps step claims through
assignee/lease owner/actor, maps run/generation owners through both prior cards
and `desired`, and maps message lifecycle through projected parties. It
cold-builds on unknown/unclassified structural claims. Details must keep that
conservative behavior while adding resource, terminal-owner, binding,
workspace, attention and retention dependencies; a claim irrelevant to roster
cards may still be relevant to details. Canonical repair, source deletion,
replication admission and local-generation changes are changes too.

A details adapter may publish `current` only after all its source families
certify admitted == projected == S and authorized exact-agent ranking/paging.
Reverse dependencies preserve both removed and added relationships. Unknown
coverage invalidates, then recomputes a bounded certified read or marks affected
sections unavailable/not current; it never advances a fence using unchecked
old sections. Unrelated source notices may advance cursors only with proof
that the held row and time/authority coverage are unchanged.

The existing incremental consumer consumes up to 256 changed-key hints per
page, rechecks authority/coverage, reuses unchanged held rows and reads changed
rows. Shared window admission coalesces compatible reads. Existing fallback
rereads are coalesced to 1.5 seconds, non-incremental clock refreshes occur every
thirty seconds, and eight-second socket pings do not reread data. These facts
are implementation references, **not** permission to leave details apparently
current during a pending invalidation or missed deadline.

Proposed budgets:

- Retain the advertised maximum **16 subscriptions per socket**, including
  details, conversation and terminal screens. A details subscription consumes
  one slot, not one slot per section; at most four details subscriptions per
  socket. Older daemons advertising no collections capability retain their
  existing eight-slot behavior and do not support this new collection.
- Existing generic collection windows remain 1–200 rows. Details accepts only
  `limit:1`; nested section/page ceilings are 200, not 200 agents each with 200
  nested rows. Default section windows and cursors are held only for the selected
  row. Avoid a fleet scan to find this exact ID.
- One row/frame, including envelope and metadata, must fit the existing
  **1,048,576-byte** client response bound. Section ceilings total 784 KiB,
  leaving envelope/metadata space. Apply the total bound as well as each section
  bound; byte-bounded pages stop at whole rows and report `has_more`. A single
  oversize row/group is explicitly unavailable; do not truncate its fields.
- At most one in-flight details read per subscription; keep the existing
  sixteen physical collection-read ceiling per socket, including canceled work
  still finishing. Replacing/unsubscribing discards old results and releases
  held state; pending replacements wait for a slot without blocking commands
  or ready conversation/screen frames. A dirty read schedules another bounded
  read, not a parallel reader for every claim.
- Sharing is allowed only at identical S/T, local coverage generations,
  selectors, page bounds and authorization/redaction identity. It is
  snapshot-certified reuse, not an age-based result cache. Do not retain all
  historical rows or expose another caller's environment values.

Cost is nonzero: source/authority checks, dependency tracking, bounded joins,
serialization and diffing remain. These measurements do not establish a CPU
budget, nonempty-queue bound or first-frame latency target; the maintainer must
choose those before implementation claims performance success.

## Paging without silently mixing snapshots

Every paged section returns `items`, `limit`, `has_more`, `next_cursor`, and
`cursor_expires_at`; bounded nested queue lists have equivalent page metadata.
A proposed additive HTTP details-section continuation read accepts the agent,
section and opaque cursor and uses the same authorization and freshness rules.
It does not create a separate live collection per section. Cursors bind the
agent, section, filters/person, page size, order, caller authorization/redaction
identity, S/T and the certified relevant-source/local-generation cut. Maximum
lifetime is the existing five-minute cursor bound, shortened by the earliest
source/lease/retention deadline. Denied sections never get cursors.

A continuation is accepted only if the relevant source and time/authority
coverage are unchanged. It returns its original fence, explicitly labelled
`basis:history` for historical transitions or as an unchanged certified
projection for other sections. Any relevant mutation, known invalidation,
coverage loss, expiry, restart or eviction returns `page-cursor-expired`;
restart from the newest full row, not a polling workaround. Unrelated commits
may leave the cursor valid only with complete dependency proof. No retained
historical SQLite snapshot is presented as current after its sources change.

On a new full-row fence, the client invalidates previously expanded sections
unless the server explicitly certifies their continuation cut unchanged.
Off-window rows loaded for inspection are not secretly subscribed: show them
as snapshot/history data and mark them not current when their certification
ends, or replace them by an explicitly requested bounded live window under the
same details subscription. User-driven continuation is allowed; periodic
re-fetching to simulate a missing stream is not.

## Authorization and additive compatibility

- Baseline details requires `read.projections`, rechecked for each read/frame
  and continuation. No trusted-local-only path is required. Selected-person
  attention uses `person_filter` and the same paired person authority as today.
- Default normalized bindings expose env names only. Optional values require
  both `read.projections` and `read.declarations` and an explicit request;
  grants/redaction are part of subscription/cache/cursor identity. The existing
  `agent-declarations` operation retains its broader scope requirement.
- Terminal inventory uses projection scope. Terminal screens still require
  `terminal.read` plus attach capability; input/resize remain separate control
  actions. A details read never grants attach or control authority.
- No details action weakens existing operational, revision, owner-generation,
  runtime-incarnation or reviewer fences. Source failure is not an authorization
  fallback. Revoked pairing/delegation clears access, not just freshness.

Advertise an optional `agent_details` capability/version with selector,
section bounds, continuation and freshness/control-frame support. Add typed
command/resource/frame definitions and regenerated clients only in an
implementation PR. New collection/fields/operations are opt-in and additive;
existing `agents`, `work`, `attention`, `missions`, terminal/conversation
subscriptions and HTTP responses are unchanged. Unknown collection support is
refused explicitly. A web client on an older daemon shows the capability as
unavailable or uses existing independently fenced views labelled as such; it
must not claim one atomic live pane or introduce polling/stale caches to mimic
this proposal.

## Open questions for the maintainer

1. **Naming and selection:** approve `agent-details` with required exact `agent`
   and explicit `person`, or reuse the existing exact-`subject` selector? The
   proposed semantics are the same regardless of field spelling.
2. **Joint local fence:** should owner observation receipts become transactional
   source rows, or should a certified local-generation vector extend the fence?
   Which existing local overlays can participate without weakening atomicity?
3. **Observed worktree ownership:** approve extending `workspace.observed` or a
   versioned successor, and identify the owner-side runtime cwd/event source and
   Git/filesystem watcher coverage. Until coverage exists, approve explicit
   `not_current/observation_only` or unavailable cwd/dirty/branch/head, never
   configured-state substitution.
4. **Freshness delivery:** approve capability-gated `invalidated` control frames,
   per-group provenance and exact deadline wakeups, including the requirement to
   mark held data not current during coalesced reads and disconnects.
5. **Attention association:** approve the targets/requester/W(A)/R(A) union,
   shared-run duplication and exclusion of mission-only/transitive matches and
   agent-owned faults. Which complete attention families can certify this seam?
6. **Bounds and paging:** approve four details subscriptions per socket, proposed
   section byte/row bounds and the additive continuation operation. Should an
   expanded section be a bounded held window or snapshot-only inspection?
7. **History retention:** keep seven days / 200 transitions with continuation
   only inside retention, or change retention in a separate storage proposal?
8. **Performance acceptance:** specify first-frame/change latency and bounded
   source work targets, especially for nonempty queues; existing measurements
   provide no SLO or WebSocket/CPU benchmark.
9. **Normalized binding policy:** confirm applied-definition exposure under
   projection scope, optional env values under declaration scope, and the split
   between declared account/terminal/authority relationships and observations
   of effective state. No additional secret/environment surface is intended.

## Non-goals

No implementation, live-daemon changes, builds, load tests, polling workaround,
age-based cache, terminal-screen join, provider refresh API, full resource or
subagent history, or expanded audit retention. Source references establish
what exists; every new behavior above is a proposal requiring implementation
and focused correctness/authorization/coverage tests in a separate change.
