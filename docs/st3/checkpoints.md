# Checkpoints and trimming

A fleet's replicated claim log only grows, because every node holds every claim. Most of what it
holds is old observation: a seat's state a minute ago, a diagnostic that has since repeated, a usage
reading that a later reading includes. A checkpoint is how every node of a fleet agrees to delete
that history together, without changing the graph, what any reader returns, or what peers believe
each other holds.

This document explains how a checkpoint works and what trimming drops, with examples. It describes
the code in `crates/smallclaims/src/store/checkpoint*.rs` (agreement, proof, trim) and
`crates/st3/src/store/checkpoint_rules.rs` (the rules for st's own claim kinds). Operating commands
are in [Fleet replication](replication.md#checkpoints). Read this before changing a rule, the
agreement, or the trim.

In one sentence: **a claim is dropped only when a later claim of the same slot, which is kept,
replaces it for every reader, and only after every machine in the fleet has proved that on its own
copy.**

## The life of a checkpoint

A checkpoint is named for a UTC day, such as `checkpoint/2026-10-01`. Its **cut** is the start of
that day, and it covers every envelope the node accepted before the cut.

```text
cut           due           sealed        verified        stable         trimmed
D 00:00Z      D+2 00:00Z    everyone      everyone        a certificate  everyone
                            same set      same drop       exists         deleted it
```

**When it is due.** A checkpoint is due two days after its cut, so every claim in it is at least two
days old. A node only works on the newest due checkpoint: when `D+2` arrives it seals `D`, and an
older checkpoint that never became stable is abandoned, because the newer sealed set contains it.
`newest_due_cut` in `checkpoint.rs` is the one place that says which.

**Who must agree.** Every participant. A participant is every writer this node has ever heard of:
the writer of any envelope or tombstone it holds, its configured peers, every member of the
membership fold, and every writer named in a checkpoint claim. Two kinds of writer are not
participants: one that left the fleet (`st fleet leave` drains everything the writer wrote to a
member first), and one a person excused (below). Removing a machine does not take it out. A
checkpoint is not stable until every one of those writers has verified, however many of them are
away.

**Seal.** A node seals the due checkpoint when it is not catching up with any peer and its set of
envelopes before the cut differs from what it last sealed. It publishes a `checkpoint.sealed` claim
carrying a digest of those exact envelopes, the sorted participant list and the digest of its rules.
Sealing is also a promise: from then on the node never writes anything dated before the cut. Every
write path clamps its time to the highest cut the node sealed, so a node with a slow clock cannot
add history underneath a checkpoint. A node may reseal while the seals disagree, for example when a
late envelope arrives. Once it has verified a checkpoint it never reseals it.

**Verify.** When every participant's newest seal carries the same sealed digest, participants and
rules digest, each node plans the drops (below) from the claims in the sealed set. It then **proves**
the plan on a copy of its own store: it keeps only the sealed claims, projects the graph and reads
every affected subject, deletes the planned claims from the copy with the same code a real trim uses,
projects and reads again, and requires the two results to be identical. If they differ it publishes
nothing, writes a `checkpoint-proof-failed` diagnostic and the checkpoint waits, so a rule bug never
reaches a live store. If they match, the node publishes one `checkpoint.verified` claim with the
digests of the drops, the kept claims, the graph and the readers' answers. A node verifies a
checkpoint once.

The rules identity covers projection inputs and replay as well as retention. Version 11 includes
`arrangements` and `arrangement_registers`, even when empty, and rebuilds them from sealed claims.
Version 12 ages out the sekrets claims written before they became local observations.
Version 13 makes status-history retention use the reader's source selection, excluding stamped
heartbeats before finding transitions. Legacy transitions that the reader exposes remain retained.
Version 14 adds raw and live ordered memberships, live counters, their authoritative member
lifecycle dependencies and the shared reverse-edge projection to proof/replay. Absent winners,
hidden memberships and folder tombstones remain retained: no ordered-membership drop rule exists.
Retiring a member changes visibility, not its retained pair position; redeclaring a reusable stable
identity can restore that position.
Version 15 distinguishes explicit placement migration: the arrangement witness includes its
retained authority marker/source heads and membership witnesses include the original placement
winner bytes/revisions plus live/lifecycle projections. Replay translates retained legacy claims
without fresh winners. No additional claim-drop rule is introduced.
Repairing a retained member's selected claim refreshes its lifecycle dependency from
unrepaired authority in the repair transaction, so incremental and replay witnesses agree.
Local membership repair watermarks survive projection rebuilds but are excluded from
shared checkpoint/digest answers.
Different builds' rules digests must match exactly, not by version ordering: a mixed-version
fleet waits at sealing until its participants use compatible rules, including during rollback.
Verification also refuses seal terms whose rules digest differs from the running build, without
consuming the node's first verification. The normal step replaces terms only for the newest due
cut; quiet retries there do not establish verification of an older cut.

A status-history proof mismatch logs bounded source diagnostics from the same scratch proof reads:
the checkpoint, seal row, cut, build and rules/drop identities, array lengths and first differing
index, and at most three items on each side for at most three subjects. Item metadata identifies
the source claim, canonical position, state, incarnation, observation time and reset/drop status.
It excludes content and stays outside reader answers and certificate digests. No additional copy
or proof pass is made to collect it.
Failure logging uses the existing bounded diagnostic limiter with a fixed stage/code bucket;
repeated proofs within 60 seconds suppress output and report the suppressed count on the next
allowed failure, with no scheduled flush or retry. Canonical source keys are cloned only for the
reader's bounded history items during the proof and discarded afterward.

**Stable.** A checkpoint is stable when every participant has published a verification that names
the same participants and carries identical digests. Stability is a pure function of claims, like
the membership fold: no clock, no order, so every node that holds the same claims reaches the same
answer. If two nodes planned different drops, their digests differ and the checkpoint never becomes
stable; the next day's checkpoint tries again. `st replication checkpoint status` names the digest
that differs.

**Trim.** Only a stable checkpoint is trimmed, and each node trims the same checkpoints in the same
order. A node that did not take part (it joined later, or was excused and came back) adopts the
checkpoint instead; see [Trimming](#trimming).

The daemon does this work in the background every ten minutes, on a thread that never holds the
store's writer for long. `[checkpoint] enabled = false` in a node's config stops that node
sealing, and since every participant must seal, that stops trimming for the whole fleet.

## What a rule may drop

A checkpoint does not decide what is old. The rules in `checkpoint_rules.rs` do, and a rule exists
only for a kind that was audited against every reader of that kind. Everything else is kept.

### Slots and witnesses

A **slot** is the part of a claim that one reader answer depends on, such as (subject, incarnation)
for a harness observation, or (subject, request) for a subscription deferral. Within a slot the
claims form a sequence in **canonical order**: accepted time, writer, batch sequence, position in
the batch, claim ID. The order never depends on when a claim reached a node.

For each slot the rule names the claims to keep. Everything else in the slot is a candidate. A
candidate is dropped only if it has a **witness**: a later claim of the slot that is kept and sets
every field the candidate sets. Most readers fold a kind last-writer-wins, field by field, or take
the newest claim, so a later claim with every field makes the earlier one invisible. That holds
whatever arrives afterwards: a late claim that sorts before, between or after the two can change
what the fold ends with only by replacing the witness, which is also what it would have done to the
dropped claim. A kind that clears all of its fields on every update (a state transition) is
witnessed by any later kept claim of its slot.

Every rule of every kind except `loop.state` also keeps the newest claim carrying each field, so a
field set only once still has its last value.

### Guards

These claims are never dropped, whatever a rule says:

- a claim whose actor is a person (`person/*`);
- a claim of a kind that is not plain append or state transition, because those kinds' validation
  reads them (`Once`, `OncePerActor`, `OncePerAttempt`);
- a claim whose record is not valid: invalid, unknown or repaired, or the replacement of a repair;
- a claim a projection row references by ID: desired state, mission definitions and revisions,
  document bindings;
- a claim another claim in the sealed set cites as evidence, or that a mission run pins as an input;
- a claim held in two envelopes, and a claim sharing an operation with a kept claim, so an
  operation is wholly kept or wholly dropped;
- each writer's newest envelope in the sealed set, because sequence numbers and heads read it;
- a `harness.observed` claim without an `incarnation_id`, because an older reader falls back to
  arrival order for it.

Envelopes go whole or not at all. An envelope is signed as a unit, so it is deleted only when every
claim in it is dropped; if one claim must stay, all of them stay.

The planner then runs the witness check and these guards to a fixed point, since keeping one claim
can remove a witness another drop relied on. The drop set is a pure function of the sealed claims
and the rules; it reads no clock, no local observation and nothing outside the sealed set. That is
what lets every node compute the same plan.

### The rules

`RULES_DESCRIPTION` in `checkpoint_rules.rs` is the canonical list. Its hash, with the rule engine
version, is the `rules_digest` in every seal, so nodes agree on a checkpoint only when they run the
same rules. In summary:

| Kind | Slot | Kept |
|---|---|---|
| `harness.observed` | subject, incarnation | the first claim; the first `ready`, `working` or `idle`; the newest; the newest non-`working` claim and every `working` claim after it; the newest carrier of each optional field |
| `harness.timeline` | subject, incarnation | the newest; only claims dated at least five days before the cut go |
| `loop.state` | subject | the first and last claim of each run of equal (status, round); the first claim carrying the loop's items |
| `subscription.mission-deferred` | subject, request | every claim while the request is open; once a start, failure, cancellation or release closes it, the newest |
| `observer.observed`, `daemon.diagnostic`, `transport.observed` | subject (and code, or origin) | the newest |
| `resource.observed` written by an observer | subject | the newest |
| `runtime.action.*` with no actor, `render.applied`, `runtime.readiness-deadline-reached` | subject (and action, incarnation, status) | the newest; only claims dated at least five days before the cut go |
| `sekret.called`, `sekret.exited`, `sekret.refused`, `sekret.changed` written before they became local observations | subject | the newest; only claims dated at least five days before the cut go |
| `harness.limits` | subject | the newest |
| `harness.usage`, response rollups | subject, incarnation, model, account, run, step, host | the last snapshot of each UTC hour in the seven days before the cut, the newest snapshot before that window, and the newest of all |
| `harness.usage`, session cumulative | subject, incarnation | the newest and the last claim of the largest total |
| `harness.usage`, context occupancy | subject, incarnation | the newest |

Kinds the rules do not name are never dropped: missions, runs, steps, work claims, gates, messages,
attention, membership, desired state, checkpoint claims themselves and every durable fact. The same
goes for a person's claim of any kind.

**Why the "five days" rows.** These kinds are local observations that the owning node's own log
already forgets after seven days. A claim is dropped from the replicated log only when it is at
least five days older than the cut, and a checkpoint is due two days after its cut, so it is at
least seven days old by then.

**Why `work.renewed` is not in the table.** It is the case that taught the rules what "witness"
costs. A late step claim can land between any two lease renewals, so no renewal has a witness that
holds for every arrival order, and the step-timing fold needs every renewal. Every renewal stays.

## Worked examples

The names here are invented. Each example's outcome is checked against the planner by a test in
`crates/st3/src/store/checkpoint_tests.rs`: `the_documented_examples_drop_what_the_document_says`
for the harness observation, the renewals and the guard, and
`usage_rollups_keep_hourly_ends_in_the_window_and_one_baseline_before_it` for the usage rollup.

### A harness observation

A seat `agent/alder.worker` runs one incarnation, `inc-1`, and its harness reports nine
observations. Each carries the same fields, as a real one does:

| # | State | Kept? | Why |
|---|---|---|---|
| 1 | `starting` | kept | the first claim of every incarnation, so the set of incarnations is exact |
| 2 | `ready` | kept | the first `ready`, `working` or `idle`: "was this incarnation ever ready?" asks for it |
| 3 | `working` | **dropped** | after claim 7 is kept, claim 3 can never be the first `working` after the newest other state |
| 4 | `idle` | **dropped** | replaced by claim 7, which is a later non-`working` claim |
| 5 | `working` | **dropped** | as claim 3 |
| 6 | `working` | **dropped** | as claim 3 |
| 7 | `idle` | kept | the newest non-`working` claim: "working since" is the first `working` after it |
| 8 | `working` | kept | the first `working` after claim 7 |
| 9 | `working` | kept | the newest claim, and the newest carrier of every optional field |

Four claims go, and the five that stay answer every question the nine did. Claim 8 and claim 9 both
stay because a late `idle` observation, written by another machine, could arrive and sort between
them. Then the first `working` after the newest non-`working` claim would be claim 9, not claim 8,
and the kept set still answers it. The rule keeps every `working` after the newest other state for
exactly this reason.

Had claim 4 been the only claim that carried a `reason` field, it would stay too, as the newest
carrier of that field.

### A usage rollup

Response rollups are cumulative snapshots: each carries the series' running total, and a usage query
for a period subtracts the snapshot at its start from the one at its end. Take one series (one
account on one step) with these snapshots, where the window is the seven days before the cut:

| Snapshot (running total) | When | Kept? |
|---|---|---|
| 100 | window start − 3 h | dropped |
| 200 | window start − 2 h | dropped |
| 300 | window start − 1 h | **kept**: the newest before the window, the baseline for a period that starts at its edge |
| 400 | window start + 10 min | dropped |
| 500 | window start + 20 min | dropped |
| 600 | window start + 50 min | **kept**: the last of its UTC hour |
| 700 | window start + 65 min | dropped |
| 800 | window start + 90 min | **kept**: the last of its hour, and the newest of all, so the series' total is exact |

Five claims go. A period that ends on any hour boundary in the window, or at the edge of it, reads
the same value, and the lifetime total is unchanged. Hourly history older than the window is gone from
the replicated log; every response is also exported to OpenTelemetry when `[observations.otlp]` is
set, and that export is the history beyond the window. Another account on the same step is its own
slot and keeps its own newest snapshot. A legacy per-response claim has no rule and stays, since
every usage read sums those.

### A kind that is never dropped

Step `step-run/example/build` renews its lease three times: `work.renewed` at times 1, 2 and 3, each
extending the expiry. Each renewal supersedes the one before it, so it looks like a dropped claim.
It is not, because `work.renewed` has no rule: the step-timing fold closes an interval when the next
event arrives after the current expiry, and whether a late claim lands inside or outside an interval
depends on every renewal. All three stay, and so does everything else about the step.

### A claim a guard protects

A daemon writes three `daemon.diagnostic` claims with code `slow-request` on `daemon/alder`; a
person wrote the first, `person/avery`. The rule keeps the newest per (subject, code), so the second
looks droppable, and it is: the third replaces it. The first is a person's claim, which is never
dropped, so all of the first and third stay. Only the second goes. If a later claim cited the
second as evidence, the evidence guard would keep that one too.

## Trimming

A stable checkpoint is trimmed in two steps, and a crash between them is safe.

1. **Tombstones, in one transaction.** For every dropped envelope and claim the node records a
   tombstone: writer, sequence, hash and, for a claim, its ID, subject, kind, actor, predecessors,
   operation and request digest. It marks the checkpoint `trimming`. The tombstones, not the
   deleted rows, are now what the node's replication inventory lists for those envelopes.
2. **Deletions, in chunks.** For each dropped envelope the node deletes its claims, their events,
   records, signatures, operations and the envelope row. After the last chunk it marks the
   checkpoint `trimmed`.

A crash leaves either nothing recorded, in which case the next pass starts again, or every
tombstone recorded with some rows still present, in which case the next pass deletes what is left.
At every point the node's inventory, authority digest and graph are the same, so peers cannot tell a
node mid-trim from one that has not started.

**Why peers do not send the dropped claims back.** The tombstones keep each dropped envelope's
identity in the inventory, so two nodes that trimmed and a peer that has not still have identical
inventories and nothing moves. A tombstoned envelope is never sent, and one that arrives is not
stored. A claim ID with a tombstone still counts as existing for evidence checks, and ancestry walks
through its predecessors. A retried request for a dropped claim is answered with the claim's ID
rather than written a second time.

**What does not change.** The graph and every reader's answer at the current index. A read at an
older snapshot, or for a dropped claim's history, answers from the kept claims. Snapshots and page
cursors taken before a trim expire through the existing "snapshot changed" path, because the last
transaction of a trim moves the committed index forward by one.

**If a deletion would change the graph.** Every chunk checks, in its own transaction, that the
graph generation did not move. If it did, the chunk is rolled back and the checkpoint is marked
`graph-changed`. The node writes a `checkpoint-trim-graph-changed` diagnostic and seals nothing more
until a person runs `st replication checkpoint resume --reason ...`. The proof makes this
impossible for a correct rule, so it signals a bug, not a condition to retry.

**A node that did not verify.** A node that joined after the checkpoint, or came back after an
excusal, sees the checkpoint in a peer's inventory. It fetches the peer's manifest (every tombstone,
plus the signed seals and verifications), recomputes the checkpoint's drop digest from the
tombstones and rejects the manifest if it differs from what every participant verified, and only
then records the tombstones and deletes what it holds. Peers send it the kept envelopes through the
ordinary exchange. What this node wrote itself, even dated before the cut, is an ordinary live claim
and replicates as usual.

## Excusing an unreachable member

A checkpoint waits for every participant, so a machine that is away, or whose build predates
checkpoints, holds up trimming for the whole fleet. `st replication checkpoint status` names who has
sealed and who has not, with the build each last sealed with.

Three days after a checkpoint's due date without it becoming stable, the participant that sorts
first among those that have sealed asks the fleet's person, through attention, to do one of:

- **Bring the machine back.** Nothing else is needed.
- **Upgrade it,** if it runs a build without checkpoints.
- **Excuse it:** `st replication checkpoint excuse NAME --reason "..." --as person/NAME`. The claim
  is a person's, and a node cannot excuse itself.

Excusing takes the writer out of the participant set, so the others can reseal without it. It
**fences nothing.** What the writer wrote while away still replicates when it returns, including a
person's or a mission's claims dated before a cut; those are late envelopes, kept, and a late
claim cannot make any drop visible (see witnesses above). The returning machine adopts the
checkpoints that became stable without it, and its first seal ends the excusal by itself.

`st fleet remove` is not an excusal. Removing a machine fences writes nobody else holds, which is
right only when its storage is gone, and it never makes a checkpoint stable on its own: the removed
machine must also be excused.

If a person excuses each side of a partition, each side can certify the same cut with different
participants. When the sides meet, every node applies one certificate: the one with the most
participants, then the smallest drop digest. A node that applied the other adopts the chosen
manifest, so every node ends with the same tombstones. See [Fleet replication](replication.md)
for what that forgets.

## The 2026-10-03 lesson: unindexed foreign keys

The first stable checkpoint on a real fleet, `checkpoint/2026-10-01`, dropped 147,245 claims. Two
members stalled every write for about an hour while they trimmed it. Replication receives timed out
and exchanges stopped, and a restart made it worse, because a restart runs the trim again at
startup. Two causes, fixed in two changes:

- **Unindexed foreign keys (#1103).** Foreign keys are on, so deleting a claim looks up every row
  that still references it. `operations`, `desired`, `mission_revisions` and `mission_definitions`
  had no index on the referencing column, so each claim delete read every operation: 148,025 on one
  member, 25.7 ms per claim instead of 0.05 ms. The four columns, and the three other unindexed
  foreign keys, now have indexes, and a test fails if any foreign key column does not lead an index.
  Add the index in the same change as any new foreign key.
- **Chunks too large for the writer (#1104).** Each trim chunk deleted up to 2,000 envelopes in one
  transaction, and with `checkpoint_claims` also unindexed by envelope, a chunk held the writer for
  about 85 s. Every write waited behind it. A chunk now deletes one envelope at a time and commits
  once it has run for 50 ms (`TRIM_CHUNK_BUDGET`), so the writes queued behind it run before the
  next chunk takes the writer. Cursors carry each tombstone scan across chunks, so the trim reads
  every tombstone once, and `checkpoint_claims` is indexed by envelope.

The rule that follows: **a trim shares the store with live writes, so no transaction in it may hold
the writer for longer than the budget, whatever a row costs to delete.** The time budget, not the
row count, is the bound. The test `a_trim_never_makes_a_write_wait_long` in
`crates/st3/src/store/tombstones_tests.rs` trims a production-sized history (150,000 claims and
operations) with every row made slow, and fails without the budget.
Its setup inserts the history in two bulk statements: inserting each row separately in one growing
transaction repeatedly spilled SQLite statement journals and sustained 42–51 MB/s of temporary
writes before the trim even started (#1145). The Linux regression
`production_history_has_bounded_journal_writes` forces spill with a small cache and bounds the
setup's write-syscall bytes, without reducing the production-sized trim proof.

## Where the proof lives

| What | Where |
|---|---|
| Due time, names, the dry-run plan | `crates/smallclaims/src/store/checkpoint.rs` |
| Participants, seals, verifications, stability, excusal, attention timing | `crates/smallclaims/src/store/checkpoint_agreement.rs` |
| Tombstones, trim, manifest adoption | `crates/smallclaims/src/store/checkpoint_trim.rs` |
| The rules, the guards, the planner, the reader digest | `crates/st3/src/store/checkpoint_rules.rs` |
| Rule examples as tests | `crates/st3/src/store/checkpoint_tests.rs` |
| Trim, tombstone and replication tests | `crates/st3/src/store/tombstones_tests.rs` |
| Agreement and convergence tests | `crates/st3/src/store/checkpoint_agreement_tests.rs` |

A new rule changes the rules digest, so it takes effect only once every participant runs it. Before
adding one, list every reader of the kind, give each kept claim a reason in terms of a reader's
answer, and add a test in `checkpoint_tests.rs` that names the dropped and the kept claims, as the
examples above do.

The observed seat status history rule (rules version 9) preserves the last 200 canonical
transition and runtime reset sources per seat within seven days before the cut, across runtime
incarnations. It also preserves the start of the current observed state, including idle or
blocked states older than that window, so trimming cannot reset `since`. Existing operational
witnesses can retain other claims; the client read independently enforces its seven-day/200-item
bound. Checkpoint tombstones make unprovable completeness explicit instead of treating deleted
observations as evidence of continuity.

Rules version 10 also treats native credential refusal and recovery as observed status
transitions within the same seven-day / 200-transition cap. It preserves the beginning
of the current credential episode and its latest native evidence fields, so trimming
cannot clear a login refusal or reset its start time. Checkpoint participants must run
matching rules before verifying a new certificate.

During a rules-v9 to rules-v10 rollout, participants on different versions can seal
identical inventories with different rules digests. They cannot verify that cut together,
so checkpoint verification and trimming stall fleet-wide until all participants use v10.
Replication and ordinary work continue; the mismatch does not authorize dropping data.
Coordinate the participant upgrades before resuming checkpoint verification. Previously
verified certificate terms stay unchanged.

The first observation on upgrade can reset `since` once when the legacy snapshot has no
`provider_auth` field and its successor explicitly records null. That is an evidence-shape
change, not proof that a login succeeded or that a runtime restarted. Later unchanged
observations preserve `since`; a held credential refusal persists until positive recovery.
