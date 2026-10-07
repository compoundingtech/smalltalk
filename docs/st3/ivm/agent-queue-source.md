# Agent card queue dependency

`store::agent_queue` composes into the agent-card Installer Operator. It has no
independent view registration, root, publisher, source capture or production
reader activation. Its functions require the owner's opaque `install::Namespace`.
Every component table, index, mutation, read and reclamation uses that namespace.
The owner must include this implementation in its operator fingerprint.

The bounded selected output reproduces `Store::agent_work_queues`: current and
upcoming previews, active and queued counts, and next work. It retains the full
unfiltered ancestor and descendant context before selecting previews. The
selected labels reproduce the existing raw step label fields from complete
captured projected rows. These fields do not grant admission or authorization.

## Complete inputs and composition

After `create_schema`, dispatch complete replacements through `replace_step`
(step_runs, subject PK) and `replace_run` (mission_runs, bare id PK). All source
SQL columns are retained for selected labels; no installation callback or read
fetches newer projected rows. PK reassignment requires old-key retraction and
new-key insertion. Changes to run ownership/current generation enqueue only
that run's old/new generation worklists. Assignee and claimant changes invalidate
both old and new public agents. No run_generations dependency is inferred here:
the existing queue reader uses the projected current-generation selector.
The full agent card's operational state has its separate canonical dependency.

`replace_claim` accepts the owner's normalized canonical row:
`{id,subject,kind,rank:[byte,...],body:<complete claim JSON>,eligible:bool}`.
Rank must be the full `canonical::sortable_key`; local store indices are never
canonical ranks. The exact input registry is `step-run.carried`, `work.claimed`
and `agent.queue.moved`. Carried and claimed retain repaired originals, while
moves exclude repaired originals. Corrections to canonical metadata or repair
eligibility must redispatch old/new replacements. `eligible:false` retracts the
old input. Claims and canonical/repair metadata capture belong to the shared
source owner, including checkpoint tombstones and unresolved causal inputs.

Call `drain(tx, namespace, captured_time, limit)` with 1..=1024 affected items per
page. It returns processed count, cleanliness and Complete/Unsupported coverage.
Run-generation, descendant-context and rank fanout use durable bounded cursors.
The ordered run component applies an append move to one ordered node; a late
canonical input undoes and replays only the affected durable event suffix in
bounded pages. Partial work rejects reads. Captured time cannot regress. Lease
expiry is inclusive, with an indexed first-deadline seek and writer maintenance.

The owner must map Unsupported to its source-gap/Root/Views fence while allowing
source admission to commit. Storage errors remain transaction errors. Ancestor
paths longer than 1024 UTF-8 bytes, ambiguous multiple eligible carries for a
single current step, malformed SQL/canonical shapes and exhausted ordered keys
must not receive a complete certificate. The native supported carry domain is
one eligible ready claimant after the latest canonical work.claimed. Sparse or
legacy carried bodies ignored by the existing reader remain ignored.

Read `rows(connection,namespace,selected_agents,captured_time)` for <=501 public
agent IDs. It performs one selected query, indexed counts, and two indexed
five-item prefixes per agent; `labels` batches <=5010 selected subject IDs in one
query. `dirty_agents` pages <=1024 public IDs and `acknowledge` clears only IDs
whose recomposed card output has committed. `next_deadline` is a scheduling hint;
reads still require the owner's current ready Root, complete source cut,
component cleanliness, all other card coverage and same-snapshot authority.

Reclaim only a retired namespace through `reclaim(tx,namespace,limit)` with a
total deletion budget of 1..=1024, including trigger-caused deletes. Root custody
and retirement prevent concurrent producers from resurrecting its worklists.
No GET seeds, flushes, scans history or advances source coverage.

The test-only full source extractor and empty-source Namespace token establish
real Store reader parity and component controls. They are not a production
source certificate or populated installation proof. Production source capture,
checkpoint/epoch recovery, total fanout/time budgets, full public-card parity,
provider/client delivery and actual activation remain the composing owner's
qualification work.

Local controls: nine ordered-component tests cover canonical ties, future-clock
joins, ignored moves, late prerequisites, bounded suffix rollback/reopen,
namespace isolation, actual query plans, one-node append/no-op duplicates,
permuted real replication, randomized full replay parity and explicit key
exhaustion. Seven real Store queue controls cover populated seven-run counts and
five-item previews, selected label parity, terminal membership, nested selectors,
inclusive native claim expiry, generation replacement/carried priority, source
rollback/unsupported shapes, two-namespace total deletion budgets, preview query
plans and label-only public-key invalidation.
