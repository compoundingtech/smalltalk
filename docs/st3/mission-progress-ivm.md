# Prepared keyed mission progress counts

`store::mission_progress_ivm` prepares `st3.mission-progress-counts.v1`, the raw
current-generation `total` and `done` counts used by mission progress previews.
It preserves `COUNT(*)` and `SUM(status='completed')`; effective step state,
preview rows, full mission cards, attention, queue order and action authority
need separate complete dependency coverage. Default Store construction and
readers do not register or activate this relation.

Source capture uses a keyed inventory of each step's run, generation and binary
completion bucket. Insert, removal and changed membership adjust at most the old
and new counter keys and queue the affected run IDs. Non-completion status
changes and identical updates leave counters unchanged. Each output maintenance
reads one current-generation header and at most two counter buckets; unchanged
counts leave its output row and view generation unchanged. A run with no steps
still has a zero-count output row. Current-generation changes and run removal
invalidate the owning run independently of step mutations.

The source owner explicitly registers `definitions()`. Fresh empty source capture
is complete; populated source capture is fenced. `seed_page(tx, limit)` resumes
durable indexed step and run cursors, processing at most `limit` source rows per
page (1 through 1024). A separate run cursor includes existing zero-step runs.
Triggers capture concurrent changes, including inserts before the cursor.
`backfill_page(tx, views, limit)` maintains at most `limit` output keys while the
primitive view remains fenced. Neither function publishes Ready or certifies a
claim frontier. SourcePending with a stored Ready flag is not a backfill fence.
The shared installation owner must certify complete source projection, finish
both capture cursors, drain pending keys, and publish the initial availability
boundary using the foundation lifecycle. There is no read or startup seeding.

For an installed complete source, `flush(tx, views, captured_time, limit)` drains
at most `limit` affected run keys through local changes in the same writer
transaction. It requires primitive readiness and complete inventory capture;
the owner supplies the complete certified source cut. The function does not
infer projection coverage from a largest log index. Partial draining makes
`rows` unavailable even if primitive readiness still says Ready. Rollback
restores inventory, counters, pending keys, outputs and generations together.

`rows(connection, views, &[public_run_id])` accepts at most 501 selected run IDs
and reads their output rows with one indexed query. Values contain the public
current generation ID and unsigned total/done counts. These raw counts have no
clock deadline; the captured writer time belongs to invalidation boundaries.
A missing output proves neither authorization nor an effective terminal state.
The caller must compose selected counts with same-snapshot generation, effective
preview, authority and other certified dependencies before publishing a card.

The seven controls compare against raw COUNT/SUM on real Store projections,
including old/new run and generation changes, zero-step runs, bounded partial
flush, rollback, output-write witnesses, explicit populated installation across
reopen, SourcePending refusal, shuffled/duplicate replication and retirement.
Replication controls here use the existing unsigned test exchange helper; they
do not qualify signed admission or checkpoint/digest/restore coverage. Custom
capture/output tables are opt-in local state; restored availability and complete
production hooks remain explicitly unqualified.
