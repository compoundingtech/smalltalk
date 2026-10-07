# Agent card activation contract

The public view and install operator are `st3.agents.cards.v1`. The source is
`st3.agent-card-source.v1`. The compiled operator fingerprint is
`agent-cards.v1;namespace-v1;ordered-harness-v2;current-authority-v1;queue-v1;usage-v1;lifecycle-v1;local-clock-v1;public-card-v0`.
This identifies the intended implementation; it does **not** attest that the
unfinished source families below are complete.

Publication selects an Installer namespace. Every operator-owned primary key,
secondary index, read, write and reclamation predicate includes that namespace.
The published row binding is `local_agent_card_rows(namespace, agent)` with
`agent` equal to the public subject ID. Rows carry `name`, `state`, `current`,
`key_generation` and the complete client-v0 JSON `body`. The ranked window uses
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

Claim extraction must include all same-agent intermediate claims for causal
authority, not only runtime heads. A claim affecting another agent through its
actor, message from/to, owning step/run, desired lineage or lifecycle subject
must retain and invalidate both old and new dependencies. Unknown kinds,
operation conflicts, repair exclusions and unresolved parents are explicit
inputs. Bounded pages are at most 128 rows; byte, total-work, fanout and ancestry
limits fence rather than silently truncate. Page cursors use stable source PKs,
not public ranked windows. Any additional table discovered by a lifecycle
producer expands this manifest and fingerprint before qualification.

## Remaining producer inputs

The six card families are desired/current membership; actual authority and
operational ownership; observed harness/activity/working episode; work queue and
labels; usage/todos/fault; handoff/suspension/rollout/subagents. Existing kernels
for individual families do not certify the complete public card. Legacy usage
response costs require the exact canonical floating-point fold or a bounded
suffix repair with an explicit exhaustion fence. Activity retains the existing
replica-local arrival selector and must be refreshed on prefix promotion and
checkpoint renumbering; canonical claim rank is not an equivalent substitute.

Delivery presence is currently in memory and includes monotonic time and
followed executable file identity. It requires a committed, versioned local
source sink plus deadline dispatch. A SQL capture trigger cannot cover it.
Harness freshness (strictly more than 90 seconds), native presence expiry,
startup grace, subagent expiry and inclusive work lease boundaries are captured
clock inputs. Client grants are revalidated in the same authorized snapshot;
provider notifications and installer roots grant no authority.

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
