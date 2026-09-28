# st data authority

This document classifies each SQLite table in schema version 13.

Schema version 13 upgrades schema versions 10, 11, and 12 in place.

The claim log and immutable blobs are the durable graph authority.

A claim kind of `local` retention (`st3 schema show KIND`) is an observation that only the node
that made it reads. The daemon records it in `local_observations` instead of the claim log. It gets
no batch, no envelope and no digest, so it never replicates and trimming it never changes what a
peer holds. Every node computes the same graph from the same admitted claims. The daemon trims
the table at startup and hourly: rows older than `[observations] retention` (default `7d`, at
least `1h`) and rows beyond `max_per_subject_kind` (default 20,000) go. The newest row of each
subject and kind stays. An older build that still writes such a kind as a claim replicates it as
before, and readers merge those claims with the local rows.

A kind of `system-local` retention is local when the system records it without an actor and a
claim when a person or agent writes it as its actor. The reconciler's own `runtime.action.*`
records, its starts, stop requests, deadlines and kills, stay on the node that runs the runtime,
where the stop fence, restart windows and adoption read them. A person's signal names its
requester and replicates with its result. A local observation may cite another local observation
of the same node as evidence; a claim can cite only claims.

A kind of `latest` retention is an observation that other nodes read only in its latest state. The
local log keeps every observation. The claim log gets an ordinary claim of the same kind only when
the state changes, so every node reads the current state from the latest claim, and a checkpoint
may drop the claims each newer one replaced. `harness.observed` replicates when any field other
than `observed_at_ms` changes. `harness.usage` replicates the first reading of each incarnation and
semantics, a compaction or model change, a reading at least five minutes after the last replicated
one while the harness works, and any reading while it does not. The newest pending reading
replicates when the harness stops working.

Runtime observations and harness observations remain separate claims. The status projection puts
runtime fields in `actual` and the current incarnation's harness fields in `harness`.

All other tables are indexes, projections, local capabilities, or transport recovery state.

A seat queue has no table. Each read derives the order from the `step_runs` projection and the
replicated `agent.queue.moved` claims on the agent subject, so every replica computes the same
order from the same admitted claims.

| Table | Class | Rebuild or recovery source |
|---|---|---|
| `meta` | Local store metadata | Store initialization and configured node identity |
| `batches` | Claim-log authority | Accepted local and replicated batch headers |
| `claims` | Claim-log authority | Accepted local and replicated claims |
| `blobs` | Content authority | Posted bytes, verified by SHA-256 |
| `operations` | Projection | Claim `_operation` metadata |
| `documents` | Projection | `doc.bound` claims and blobs |
| `desired` | Projection | Selected `intent.desired` heads |
| `events` | Projection | Effective accepted claims |
| `mission_revisions` | Projection | `mission.published` claims |
| `mission_definitions` | Projection | Selected `mission.published` heads |
| `mission_runs` | Projection | `mission-run.*` claims |
| `run_generations` | Projection | `run-generation.*` claims |
| `step_runs` | Projection | `step-run.*` and `work.*` claims |
| `local_work_lease_renewals` | Local operational fact | Recent quiet lease renewals; replayed over replicated claim projections and bounded by periodic `work.renewed` anchors |
| `local_observations` | Local observation log | Observations of `local` and `latest` retention made on this node; never replicated, trimmed after `[observations] retention` |
| `local_latest_slots` | Local observation log | For each `latest` slot this node writes: its last replicated observation and time, and the newest local observation no claim carries yet |
| `revision_proposals` | Projection | `revision-proposal.*` claims |
| `planning_sessions` | Projection | `planning-session.*` claims |
| `planning_candidates` | Projection | `planning-session.candidate-submitted` claims |
| `planning_previews` | Projection | `planning-session.previewed` claims |
| `idempotency` | Opaque response cache | Hashed caller keys and derived responses |
| `mission_run_requests` | Opaque validation cache | Hashed caller keys and request digests |
| `capabilities` | Local short-lived authority | Dedicated API issuance; capabilities do not replicate |
| `replica_envelopes` | Replicated authority | Authenticated outer envelopes and their exact payloads |
| `replica_records` | Admission state | Envelope records, validation results, and repair references |
| `projection_health` | Local diagnostic projection | Projection attempts against admitted authority |
| `replication_peers` | Local transport state | Last signed exchange or transport failure for each configured peer |
| `peer_cursors` | Legacy test state | The removed cursor protocol; production does not use this table |
| `peer_replica_cursors` | Legacy test state | The removed cursor protocol; production does not use this table |

The store rebuilds operation and planning projections when it opens.

Replication receipt stores an envelope before admission decodes its payload.

Admission validates each claim and blob independently. Invalid or unknown records do not enter graph projections.

Projection uses admitted claims and keeps the last good graph when one reduction fails.

`st doctor` compares the operation projection with the claim log.

An idempotent claim stores a keyed hash of the caller key in its `_operation` metadata.

`st claim --idempotency-key KEY` opts a public claim into this contract.

The claim stores the canonical request digest and canonical claim ID.

An exact retry returns the original claim.

A retry with different input returns `idempotency-mismatch`.

Conflicting replicated operations mark the affected graph subject as indeterminate.

A `record.repaired` claim names one bad record and one valid replacement claim.

Repair keeps the original record. It changes the record state to `repaired` and records the replacement reference.

The database does not store the caller key.

Eval cleanup removes every desired projection row owned by the terminal eval run.

Eval history stays in the immutable claim log.

A cleanup residue produces `eval.verdict` with `verdict=fail`.

A cleanup infrastructure error produces `eval.verdict` with `verdict=void`.
