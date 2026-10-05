# Keyed agent-card storage prototype

Source base: draft #1409 at `6fdf89ce8a1ac714226584aa09686bbadf5e589d`.
This prototype is isolated from the production writer and API. It does not claim a
compiled test, a correct migration, or a latency result.

## Ordered history and cursors

Persist an immutable balanced search tree of `(effective_name, agent_id) -> card
version` nodes. Path-copy an update so one changed card creates O(log fleet)
nodes. The root table points from each canonical source cut to one root for each
`(history, status)` variant, including `status='*'`. A read chooses the latest
root at or before its cut, seeks after the cursor's last `(name, id)`, and emits
at most `limit+1` nodes. A point lookup uses a separate subject-keyed version
index. Historical cuts and continuations refer to immutable roots. Retain roots
until their bounded cursor lifetime ends; otherwise return `page-cursor-expired`.

The card tree contains stable replicated fields. Keyed local facts hold
observation, transport, subagent/step lease state and each next expiry. A
second immutable presentation tree combines stable card versions with those
facts, ordered by `(name, id)` with roots for each final status filter. An
expiry can therefore move one card between status classes without a claim.
A cursor binds its exact presentation root ID, the source cut, local
generation, frozen read time, filter, page size and last key. An unrelated
local write creates a newer root but cannot change the old page. Deadline and
local-fact updates path-copy only affected agent keys.

These trees must be stored in the same SQLite writer transaction as the claim
projection and reverse dependencies. No process-local map is authoritative.
Store/open must check schema and root source cut; a mismatched or missing root is
not served. Startup repair works off the read path and has a separately measured
cost. Compaction retains roots referenced by live cursors and required historical
cuts. A canonical reorder or rollback invalidates roots from the earliest
changed cut, replays only affected keys using indexed dependencies, and publishes
replacement roots atomically. The old roots remain available to existing
cursors only if their source cut is still canonical; otherwise return an explicit
cursor gap.

## Candidate discovery and bounded deltas

Gather candidate agent IDs from **both** sides of the write transaction. Direct
agent-subject claims contribute that subject. A message.sent contributes old/new
`fields.from` and `fields.to` when they are agents; work.progress/submitted
contributes old/new actor. Run and generation changes seek `desired.owner_run`,
`desired.owner_generation`, and current-generation step rows, then collect
old/new assignee and lease owner. A step-row change collects only old/new
assignee and lease owner; `available_to` is an input for their queue selection,
not another roster key. Local observations, subagents, and deadlines contribute
their one agent. Queue moves contribute their subject agent. The attached SQL
lists the reverse indexes required by these lookups.

For each candidate, update field summaries in O(log history) or constant time:
latest selected claim, message/work activity maxima, usage sums, queue counts
and limited preview, current harness state and its working-run start. The
`WorkingRun` monoid in `working_run.rs` is the harness-state fold for a balanced
per-agent/incarnation canonical event tree. An insertion, deletion, or reorder
changes only the nodes on that event's path; its root gives the first working
observation after the last non-working observation. The same indexed aggregate
pattern applies to usage and activity. Comparing old/new complete serialized
card values after these bounded summaries decides whether to path-copy a card
node. Candidate discovery by itself does not touch card rows.

## Integration boundary and proof

This prototype adds no production module or shared method edit. Integration
would add projection tables/migration under `Store::open`, transactional hooks
in claim/desired/step/local-observation writers, and replace only
`client_agent_resources`, `client_agents`, `client_agents_detail`, and their
`cached_agent_resources` call path after exact diff review. Preserve #1409
scoped receipt reuse, timestamp batching and shared rollout selection.

The required proof runs a full recomputation oracle only in tests. For all 134
registered kinds plus `custom.*.*`, compare IDs, order, membership and every
public field across cold/warm, historical, status-filtered, unrelated/related
writes, expiry, replica reorder, repair, rollback, checkpoint and reopen. Assert
zero touched card/order/dirty rows on unrelated writes. At two fleet sizes and
two per-agent history sizes, record successful populated read VM steps, full
scans, touched rows, writer statements, path-copied nodes and startup time.
History-growth and writer cost are separate from fleet-growth. No budget or p99
acceptance follows from this source prototype.
