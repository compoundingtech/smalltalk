# Keyed agent-card storage prototype

Source base: #1409 at `6fdf89ce8a1ac714226584aa09686bbadf5e589d`.
This isolated source design has no production writer or API hook and no compiled
or latency receipt.

## Canonical cuts, ordered membership, and cursors

Persist immutable AVL trees of `(effective_name, agent_id) -> card version`
nodes. Each node stores its height; rotations path-copy O(log fleet) nodes and
guarantee height below `1.45 log2(fleet+2)`. The root table identifies a
`(projection epoch, store index, history, status)` cut, including `status='*'`.
A page seeks after `(name, id)` and emits `limit+1`; a detail read follows a
second persistent AVL tree keyed by agent subject from the same cut root.
Card `version` is an opaque immutable row ID, not a store index. An epoch fork
can reuse the old point root for unchanged agents; it does not copy their
version intervals. A predecessor query on `version<=cut` would be incorrect.

Local facts use immutable `(agent, version) -> facts` rows. A separate current
pointer stores the newest version and next deadline as 16-byte big-endian
u128, so its index preserves time order. A retained presentation
node names both its card and local-fact versions, so a cursor pinned before a
local update still reads the exact prior fields. Facts for observation,
transport, subagents and step leases define disjoint per-agent intervals of
final rendered status and fields over the full u128 millisecond domain. Each
interval is inserted into at most 256 canonical nodes of a fixed-depth
persistent time segment tree. Each time node has AVL ordered roots by final
status, including `*`. At frozen time `t`, a read follows one 128-level time
path, seeks after the last key in each ordered root and merges them to produce
`limit+1`. Each agent has one active interval at `t`, so the merge has no
same-agent duplicate. A mass expiry changes only which immutable roots are
chosen; it cannot trigger synchronous catch-up or return stale status. The
fixed time-depth factor and interval fanout still need measured work bounds.

A cursor binds its exact time-root ID, projection epoch, canonical store cut,
local generation, frozen u128 read time, history/status filters, page size and
last key. An unrelated claim/local write cannot change that root. A repair or
reorder fences the old epoch at its first changed store index. Old cursors
before the fence remain valid; later ones return an explicit cursor gap. A
cursor whose retained root has expired returns `page-cursor-expired`. Root
retirement time is persisted when a newer root replaces it; the newest root
is always retained. Keeping a retired root for at least the cursor TTL means
every cursor issued while it was current can finish without a read-side pin
write. Cursor TTL starts at issuance, not root creation.

## Indexed candidates and bounded field deltas

Collect candidate agents from both pre- and post-state inside the writer
transaction. An agent-subject claim contributes that agent. A `message.sent`
change contributes old/new `fields.from` and `fields.to` agents; a
`work.progress`/`work.submitted` change contributes old/new actor. Run and
generation changes seek owner-run/owner-generation desired rows and
current-generation step rows, then collect old/new assignee and lease owner.
Step changes collect those two keys; `available_to` can affect their queue
selection but is not itself a roster key. Local facts, subagents, deadlines and
queue moves contribute their own agent. `candidate_queries.sql` gives the
indexed reverse seeks. Discovery alone does not touch a card or order node.

Each candidate uses bounded field summaries: selected claim, message/work
activity maxima, usage sums, queue counts and limited previews, current
harness state and its working-run start. `WorkingRun` in `working_run.rs`
combines a per-agent/incarnation AVL canonical event tree. Insertion,
deletion or reorder recomputes O(log per-agent history) ancestors, not the
whole incarnation. The root yields the first `working` observation after the
last different state. Apply the same indexed aggregate pattern to usage and
activity. Compare old/new complete serialized card values before path-copying
an ordered node. The old `agent_working_since` parses accepted timestamps as
u128; the schema keeps decimal u128 text and orders by a separate canonical
key rather than narrowing to SQLite INTEGER.

## Repair, reopen, GC, and proof gates

All published roots, dependency edges and point trees must change in the
same SQLite writer transaction. Store/open checks schema and epoch source
digests and resumes a valid root without a fleet scan. Missing roots are built
in fixed-size writer chunks into a shadow epoch, then published atomically.
Until publication, the new cut is explicitly unready while the old cut remains
readable. This availability interval is an open acceptance gap. A canonical
reorder/rollback fences the old epoch and replays indexed affected keys into
new roots. GC marks nodes reachable from retained roots, then sweeps at most
a fixed node count per writer turn. It cannot delete a node reachable from a
still-valid cursor root. Cursor lifetime and historical retention
bound root/fact versions; measure retained bytes, startup resume, shadow
replay and per-turn GC cost independently. GC keeps the latest root and all
roots retired within the cursor TTL, then sweeps unreachable nodes in bounded
chunks. Retained bytes depend on write rate times TTL and need a measured cap;
if that cap cannot be met, first-page admission or cursor lifetime must be
revised explicitly, never by deleting a still-valid cursor root.

Integration would add projection tables/migration under `Store::open`, hooks in
claim/desired/step/local-fact writers, and then replace the shared
`cached_agent_resources`, `client_agent_resources`, `client_agents` and detail
paths after exact diff review. Preserve #1409 scoped receipt reuse, timestamp
batching and rollout selection. This prototype makes none of those edits.

A test-only full-fold oracle must compare IDs, order, membership and public
fields for all 134 registered claim kinds plus `custom.*.*`, cold/warm reads,
status filters, related/unrelated writes, expiry, canonical reorder, repair,
rollback, checkpoint and reopen. Specifically: pin page one, update a local
fact, then assert that page two at the old cursor retains exact old fields,
status and membership. Compare `WorkingRun` against the actual Store fold over
insertion, deletion, reorder, rollback and incarnation changes. At two fleet
and two per-agent history sizes, record populated read VM steps/full scans,
writer statements/touched rows/path copies, startup and GC. The pure monoid
and SQL parse checks are source checks, not semantic or p99 acceptance.
