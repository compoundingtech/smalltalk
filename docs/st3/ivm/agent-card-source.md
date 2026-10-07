# Agent card activation contract

The public view and install operator are `st3.agents.cards.v1`. The source is
`st3.agent-card-source.v1`. The compiled operator fingerprint is the authoritative
`agent_card_source::FINGERPRINT` constant (`st3.agent-card.complete.v2`).
`complete_manifest()` binds that definition to the physical capture schema;
actual installation and reads separately require the receiver-bound
`capture_fingerprint_for(Store::origin())` source identity and source epoch.
These identifiers describe compatible code and inputs; they do not establish
complete source coverage or authorize publication.

Publication selects an Installer namespace. Every operator-owned primary key,
secondary index, read, write and reclamation predicate includes that namespace.
The published row binding is `local_agent_card_rows(namespace, agent)` with
`agent` equal to the public subject ID. Rows carry `name`, `state`, `current`,
`key_generation`, `body_hash` and the complete client-v0 JSON `body`. Internal
numeric bit evidence preserves floating-point public values across SQL JSON
storage; decimal parsing must not change a cost or percentage. A blank legacy
`updated_at` is marked `request_time` and filled from the captured frame timestamp.
That presentation step never changes row generation, status or ordering.
The ranked window uses
`(namespace, current, name, agent)` or `(namespace, current, state, name, agent)`.
No substring, generated identity, claim ID or private dependency key is a
public row ID. Name ordering uses SQLite BINARY and then the subject ID.

`local_agent_card_coverage(namespace)` records incomplete dependencies, pending
affected keys, captured evaluation time and the first outstanding deadline.
Publication and provider reads require zero incomplete/pending inputs. A root
alone cannot clear a later raw-write, source-gap, authority or deadline fence.
Installer source revisions and graph positions remain distinct; the production
owner must capture and certify a graph SourceCut separately. No MAX-index proof.

## Required paired source rows

Capture full old/new values, including deletions and identity reassignment, from
the following tables. The listed alternate unique identities must be covered
before capture can be certified. Canonical metadata corrections must redispatch
dependent claims, rather than only keys from new claim insertions.

| Table | Primary identity | Additional unique identity | Required fields |
| --- | --- | --- | --- |
| claims | store_index, INTEGER PRIMARY KEY/rowid alias | id | batch_id, subject, kind, origin, actor, body, predecessors, accepted_at_unix_ms, and both identities |
| batches | id | none | origin, replica_sequence, previous_hash, hash, accepted_at_unix_ms |
| operations | id | none | request_digest, canonical_claim_id, state |
| replica_records | record_ref | writer, sequence, envelope_hash, position | all columns, including state, claim_id, subject_hint, kind_hint, replacement_claim_id and native canonical position |
| checkpoint_claims | id | none | all columns; tombstones supply causal links, not invented claim bodies |
| checkpoints | id | none | all columns; trim/rank renumbering requires explicit epoch recovery |
| desired | subject | none | kind, revision, claim_id, body, member, owner_run, owner_generation, owner_step |
| mission_runs | id | none | all projected columns, including mission_id, status, phase and current_generation_id |
| run_generations | id | run_id, id | all projected columns |
| step_runs | subject | generation_id, step_path | all projected columns, including assignee, available_to, lease identities/deadline, title, goals, status, updated_at and constraints |
| local_work_lease_renewals | subject | none | attempt, lease_owner, lease_incarnation, lease_expires_at_unix_ms, updated_at_unix_ms |
| local_observations | id, INTEGER PRIMARY KEY/rowid alias | dedupe_key where non-null | after_store_index, subject, kind, actor, body, request_digest, observed_at_unix_ms and both identities |
| local_mailbox_owners | subject, component | none | incarnation, epoch |
| local_mailbox_bindings | token | none | subject, component, incarnation, epoch |
| local_agent_delivery_presence | recipient, driver | none | complete captured assessment and live producer certificate; source-owner schema |
| local_agent_card_clock | singleton | none | captured unsigned evaluation time, monotone revision, finite maintenance reason and same-transaction snapshot_index |

Claim extraction must include all same-agent intermediate claims for causal
authority, not only runtime heads. A claim affecting another agent through its
actor, message from/to, owning step/run, desired lineage or lifecycle subject
must retain and invalidate both old and new dependencies. Unknown kinds,
operation conflicts, repair exclusions and unresolved parents are explicit
inputs. Bounded pages are at most 128 rows; byte, total-work, fanout and ancestry
limits fence rather than silently truncate. Page cursors use stable source PKs,
not public ranked windows. Any additional table discovered by a lifecycle
producer expands this manifest and fingerprint before qualification.

## Complete card dependencies

The six card families are desired/current membership; actual authority and
operational ownership; observed harness/activity/working episode; work queue and
labels; usage/todos/fault; handoff/suspension/rollout/subagents. Existing kernels
for individual families do not certify the complete public card. Legacy usage
response costs use exact canonical suffix repair with bit-preserved sums and an
explicit exhaustion fence. Usage GET reads one published row. Its affected-write
aggregation currently admits at most 128 incarnation groups and 128 selected
rollup slots per group; exceeding either bound refuses publication.
Activity retains the existing
replica-local arrival selector and must be refreshed on prefix promotion and
checkpoint renumbering; canonical claim rank is not an equivalent substitute.
Local observations qualify only when `after_store_index <=` the captured clock's
`snapshot_index`. A namespaced eligibility flag and indexed category heads keep
future anchors out of current reads. Position changes schedule indexed ranges
with a durable `(after_store_index, observation_id)` cursor; equal-anchor groups
may span multiple pages. Extraction before the clock page starts with a zero
admission position, rather than admitting unknown future observations.

Canonical owner status/mode heads preserve the snapshot-index membership rule;
projected queue generation selection is a separate dependency. Owner and operation
reverse invalidation is durably paged. Rollout phases use fixed canonical sparse
field heads, including sticky forced/start flags and explicit reason/block clears,
before one published row is written. Literal nested `fields.fields` actual inputs
are outside the fixed-head compatibility domain and refuse publication.

Delivery presence includes monotonic time and followed executable file identity.
The retained producer registration supplies a committed, versioned local SQL
sink and captures all five drivers for each requested recipient. Its live,
namespace-wide certificate must cover the complete file footprint, with at most
64 distinct files. Both row reads and silent advances validate the live producer
boundary; selected SQL row certificates alone never establish closure. Producer
requests use indexed missing/epoch/expiry ranges, sharing a 128-candidate budget.
The source controller supplies deadline dispatch and acknowledgement after the
SQL commit. A SQL capture trigger alone cannot cover this producer.
Harness freshness (strictly more than 90 seconds), native presence expiry,
startup grace, subagent expiry and inclusive work lease boundaries are captured
clock inputs. Client grants are revalidated in the same authorized snapshot;
provider notifications and installer roots grant no authority.

The existing public snapshot fold uses admitted claims without consulting mutable
signature/verdict caches. This is an explicit unfiltered verdict admission domain,
bound to the st3 schema/claim registry digest. Verdict mutations are not assumed
immutable. Changing that admission policy requires new capture dependencies and
an explicit fingerprint/install. Repaired eligibility remains family-specific:
owned declarations and queue moves exclude repaired originals, while actual,
harness, activity, usage and provenance retain the original admitted facts.

Clock/deferred repair work enters through captured maintenance replacements and
`Operator::apply`; direct Installer generation updates are forbidden. Physical
normalization, reverse fanout, family repairs, deadline work and card writes
share one 128-item maintenance budget. Durable seek cursors let later keys make
progress. Computed queue cleanliness and committed public-card acknowledgements
are separate: removal or certified absence acknowledges queue dirtiness in the
same transaction as output. Namespace reclamation processes one family per
callback and charges final continuation deletion to its own remaining budget. Incomplete
bounded repair/producer acknowledgements temporarily refuse independent coverage.
Unsupported semantics, missing manifest coverage and raw write gaps permanently
fence the source and view until a qualified replacement namespace is installed.

The production callback selects at most the requested limit plus one (limit is
1..=200) from the namespaced ranked index, then reads only selected public keys.
Retained rows may be reused only after the current namespace, public membership,
order, status, key generation, source coverage and session authority have all
been verified in that snapshot. Ready availability changes refresh even an
empty changed-key page. Cursors are acknowledged only after delivered frames.

This contract remains unqualified until real populated-Store installation,
concurrent catch-up, rollback, raw-write gaps, trim, repair, local inputs,
deadline transitions, full public-card parity and the production provider/client
path pass their controls. There is no default registration or reader activation
in this document.
