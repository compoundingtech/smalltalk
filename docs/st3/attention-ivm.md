# Open attention operators

`store::attention_ivm` prepares two keyed families for the shared collection runtime:
`st3.attention.person-steps.v1` and `st3.attention.custom.v1`. They maintain complete
client attention rows, including person ask context, structured answers and updates,
custom reply forms and action parameters. Public IDs retain the existing source,
person and recorded episode hash.

These operators do not activate the production attention or Now collection. Human
gate reviews, planning and revision approvals, harness login and person-owned broken
gates still need their own operators and parity evidence. A ready family cannot be
advertised as a ready whole collection. Explicit closed history remains independent.

## Shared runtime integration

Compose `definitions()` into the Store's single `Arc<Views>`. The owning collection
runtime installs the event journal and owns the single Publisher. This module never
attaches a registry, Publisher, timer or worker to a production Store.

Source table triggers capture changed custom sources and person-assigned steps.
Indexed reverse dependencies follow the request, requester declaration, origin step,
owning runs, root and ancestor runs, delivery parent and desired declaration.
Canonical batch and replica-record corrections invalidate dependent sources even
without an admission frontier advance. Reassignment uses the old dependency edge
until maintenance replaces it. An unrelated write produces no attention key change.

After source projections complete, call `flush(tx, views, captured_time, limit)`
with a page size of at most 128. Output, dependencies, semantic key changes and local
source generation commit together. `clean(connection, Some(view))` is an additional
completion requirement; `flush` returning false cannot certify complete output.
The installer must prove the complete common prefix, source dependencies and all
local write hooks. A maximum claim index and `after_projection` alone do not do so.

For a fresh, unpublished installation, `seed_page` extracts at most 128 source keys
after a supplied source cursor. `backfill_page` processes at most 128 queued sources
and refuses a published family. The owner must hold the output unavailable, finish
every extraction and catch-up page, certify source coverage, and publish explicitly.
These helpers do not implement the namespace-swapping installation protocol or
authorize rebuilding a serving index. No reader or ordinary Store open calls them.

## Reads and captured time

`window(connection, views, view, person, at, limit)` returns at most 501 ranked public
rows. `row(connection, views, view, person, id, at)` seeks one public ID. Both require
matching registry readiness and no unflushed family sources in the same snapshot.
The caller must authorize the person and hold the existing collection admission
permit for the read. Global source invalidation keys are private `(family, source)`
JSON pairs; only authorized public rows and their IDs may reach a client.

Rows retain requested time and their earliest eligibility time as exact decimal
`u128` values. Person asks require both their accepted time and requested time to
have arrived. Reads use the supplied `at` throughout. They neither read the wall
clock nor mutate the index.

`clock_page` publishes bounded eligibility-key changes between supplied clock cuts.
Its owner schedules it, commits each page, and retains the continuation only after
successful commit and delivery. A backward clock requires a replacement snapshot.
There is no polling or cache fallback in this module.

## Remaining certification

Owned-set retirement currently fences the person family because its rollout
selection and runtime dependencies are not yet certified. An ancestry exceeding
256 runs also fences that family. Other families remain independent. Source repair,
removal, restore and checkpoint paths must retain the primitive's fences until the
shared installer certifies the replacement output.

The real Store tests compare complete family rows against
`client_attention_resources_at`, including actions, ordering, captured times,
answer/cancellation, custom reply and missing document recovery, actor isolation,
reassignment, concurrent creation and replication order, run closure/reopen, and
transaction rollback of output and event cursors, persisted reopen, and explicit full
projection replay. Checkpoint trimming preserves canonical episodes while fencing the
registry: reads and maintenance remain unavailable until the shared installer certifies
a replacement lifetime. Their installation helper is
test-only and cannot serve as production source certification. Full collection
activation and deployed cost measurements remain separate required work.

The installer must also capture late `run_generations`, `mission_run_deadlines` and
`mission_run_after` rows used by the joined run header; the current operator triggers
do not cover those tables. Deferred dirty sources need a continuation that can pass
a fenced family's first page. These are certification requirements before activation.
Legacy ASCII case variants in person assignments retain the full reader's SQL LIKE
selection, while person row reads continue to require an exact recipient match.
