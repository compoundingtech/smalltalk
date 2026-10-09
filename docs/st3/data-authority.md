# st data authority

This document classifies each SQLite table in schema version 16.

Schema version 16 upgrades schema versions 10 through 15 in place. Envelope payloads are stored
as their exact decoded bytes. Existing base64 TEXT rows convert to BLOBs after startup in bounded
writer-queue transactions; each transaction commits its progress cursor with the converted bytes.
Mixed TEXT/BLOB stores remain readable after interruption. This version changes no indexes, and
older binaries reject it. Version 15 added the shared canonical document `binding_key` and its
index, with existing keys backfilled once from claims.

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

Current categorical status, context occupancy, todo, workspace availability and peer connectivity
are replaceable values in `latest_values`. There is one row per subject and kind (per observing
host for connectivity). A separate SQLite connection uses a zero busy timeout and an immediate
transaction with a 100 ms SQLite progress deadline; contention drops the attempt without entering the ordered graph writer. A new
sample replaces the previous value and its local feed row. Status keeps the current episode's
start time and a current-incarnation readiness bit; it retains no transition history.

Native producers replace their local snapshots without creating publication jobs. Each fresh
snapshot gets one HTTP attempt. All current snapshots in one wake share a 100 ms deadline.
An independent publisher reads the newest source snapshots while durable accounting or timeline
publication waits. Its own wake pipe prevents the ordered drain from consuming current wakes.
Failure leaves no retry obligation; only
a subsequent source snapshot triggers another attempt. The source account is captured with
context evidence, so delayed reads cannot attribute it to a successor's account. Authenticated
fleet peers receive current values over `/v1/peer/current-value`, once per peer with a 250 ms
whole-hop bound, including the receiver's 100 ms local hop, independently of signed graph inventories.
The relay reuses Fabric listeners and HTTP connections; failed attempts discard the listener address
and leave no retry job. Current samples aimed at a peer in offline/overload backoff or with existing failed connectivity
evidence are dropped, including while its worker checks that route again; worker recovery does not
replay them. The legacy `/v1/harness-events` envelope still validates nonzero sequence and retains its
native replay and transition-wake contract. Upgraded categorical producers use the bound current
route; durable numeric and timeline publications use the legacy envelope.
Native current writes require the same kernel Unix-peer seat identity and
running incarnation as `/v1/harness-events`. A kernel-bound `starting` hint may precede the durable
runtime record; it only makes mailbox startup wait and grants no delivery or readiness authority.
Owner/incarnation binding and source revision order
reject stale deliveries. A persistent database generation orders source counters after a database
reset; ordinary reopen keeps that generation. Generation birth requires the owner's clock to advance
across resets. Restoring an older database while native producers continue running requires restarting
those producers with new incarnations before current publication resumes. There is no peer retry, acknowledgement ledger or
fallback to durable replication for these values. Current status reports source observation
time and freshness, so an offline owner or dropped heartbeat ages visibly.
Current-transition consumers read `Store::current_observation_boundary` with the feed/snapshot
inside a pinned read. Its database epoch and retired local-cursor floor make missing transitions
explicit: a different epoch, a cursor behind retired evidence, or a cursor ahead after restore
requires resync from current values. Replacing or importing a register advances the retirement
floor atomically with deleting its old feed row. This is a current snapshot/feed boundary;
selected durable subscription wakes belong to graph-watch, rather than a telemetry replay log.

Numeric `harness.usage` readings and account limits remain durable accounting facts for
`st usage` and the 95% stop. Their existing first-reading, compaction/model-change, five-minute
working cadence and stop flush remain in place. `context_occupancy` is exclusively a current
value and never enters that flush. Messages, work ownership, runtime launch/stop proofs and
replication inventories retain their durable contracts.

Current readers accept the old claim kinds and read legacy graph observations until a register
is present; a newer legacy observation remains visible if a source rolls back. Existing graph
history is retained for mixed-build replay and checkpoint compatibility in this slice. No claim
kind, schema version or client protocol version is removed here. Older peers that lack the
current-value endpoint receive no new categorical observations through graph replication; their
last observation ages until upgrade. The client status object keeps its existing fields. For a
seat using the new register, `status-history` keeps its existing response shape and returns an
empty `items` list with `complete=false`: replaced transitions cannot be reconstructed. Current
values are read at request time even when the request has a pinned durable graph index.

A node can also send every local observation to an OpenTelemetry collector. The exporter is off
unless the config names a collector:

Claude and Codex response token buckets are `harness.timeline` usage observations. They retain
the turn ID, model, agent, mission run, and step on the producing node. `local_usage_seen` makes
replayed provider responses idempotent across harness incarnations; `local_usage_totals` keeps
monotonic cumulative buckets even after the observation log is trimmed. The node replicates
`harness.usage` response rollups by agent, incarnation, model, mission step, and host. Fleet usage
queries subtract the last rollup before a period from the last rollup inside it.

```toml
[observations.otlp]
endpoint = "http://127.0.0.1:4318"             # OTLP/HTTP JSON base URL
headers_file = "/absolute/path/otlp-headers.toml" # optional, for example x-api-key = "..."
```

Each local observation becomes one OTLP log record in OTLP/HTTP JSON:

- the record's timestamp is the observation time, and its body holds the fields;
- its attributes are `st3.subject`, `st3.kind`, `st3.actor`, `st3.incarnation_id` and
  `st3.local_id`;
- the resource names `service.name = st` and `st3.node`; the scope is `st.observations`.

Claude event hooks submit `harness.telemetry` observations to their local daemon. They retain
the counter recorded at the shared hook's actual application point, bounded warning/error logs,
operation times and the hook exit code. No collector or SDK exporter runs in a hook. Submission
is best effort with a 250 ms daemon deadline and cannot change the hook result. The timer-driven
status line stays local-only; an undeclared application produces no invocation count. Driver
diagnostics and native session/resume binding keep their existing state and diagnostic paths.

The daemon exports the captured warnings as correlated log records, `hook_invocations_total`
as monotonic delta points on `/v1/metrics`, and one `st.hook.claude-observe` operation span on
`/v1/traces`. Metric labels remain the bounded hook/event vocabulary; subject and incarnation
identities appear only in logs and spans. Span IDs are stable when a batch is retried. Collector
configuration lives in `[observations.otlp]`; `OTEL_EXPORTER_OTLP_*` in a native seat no longer
controls an exporter in its hooks. The legacy st2 product's exporter remains separately testable.

Fresh native seats retain the existing ordered timeline outbox in `st-harness-events.sqlite`
for the subsequent owner-native conversation cutover. Activity, context occupancy and todo use
the current-attempt path above without outbox events. Context producers retain numeric session
usage and account-limit samples in separate `harness-accounting` events without occupancy fields;
these durable facts keep the normal request timeout, prepared attribution and retry path, and
publication fingerprints advance only after success. A source with accounting or timeline data
retains a separate durable stop control; it flushes pending numeric usage through the native
owner-fenced accounting endpoint even when the idle/ended register was dropped. A final reading
that arrives after the control remains unacknowledged until its stop flush succeeds. Admission
and provider-capacity diagnostics also retain normal timeout and retry handling outside the
current publisher. Timestamp-only state heartbeats do not duplicate these obligations. At the source, unchanged accounting does
not append another event merely because context occupancy or its record timestamps changed. The
comparison includes the owner, account, numeric values, resets and actual limit-source timestamp;
a new account-window measurement still provides fresh evidence for the 95% stop. This guard commits
atomically with the durable event and survives driver re-exec. A busy or full accounting spool cannot
advance the guard or roll back a committed current snapshot. The matching driver discards queued categorical events from
predecessor builds while retaining numeric usage and limits from older context events. Evidence expiry
wakes one derived unknown attempt; a concurrent heartbeat supersedes it. Expired snapshots do not
re-arm the driver's expiry deadline. The remaining timeline outbox preserves source runtime,
source account and prepared attribution across replay until that separately reviewed removal.
Accounting durability remains required after that removal.
Its 64 MiB limit cannot block a current-value write. Native transcripts and session bindings
keep their existing source paths. This change does not restart shared services or seats.

The exporter keeps its cursor in `meta` and moves it only after the collector accepts a batch of
at most 512 observations, including every log/metric/trace request that batch needs. Delivery is
at least once: partial acceptance can repeat logs, delta points or spans on retry. A collector that is down delays export
with backoff up to five minutes and never blocks a write. Observations trimmed before export are
counted in the daemon log.

Codex and OpenCode delivery holds are durable `delivery.hold` decisions on the seat, with
an actor, reason, and explicit expiry. They are separate from reachability, observed activity,
and the harness's own approval/compaction holds. Inspect, set, and release them with:

```sh
st agents hold agent/example/worker
st agents hold agent/example/worker --for 15m --reason "Quiet interval" --as person/alex
st agents hold agent/example/worker --release --reason "Resume delivery" --as person/alex
```

`GET /v1/delivery/hold?subject=agent/example/worker` returns the current decision and whether
it is active. `POST /v1/delivery/hold` is the dedicated operation; ordinary claim submission
cannot write it. A person may control a seat, and a seat may control itself. Setting a new hold
requires a declared Codex or OpenCode seat, the harnesses whose former DND behavior this replaces.
A release records expiry zero; a hold requires a future Unix-millisecond deadline. Decisions
replicate and survive daemon/driver restart. Expiry enables delivery without another write.

Native wrappers no longer create or refresh legacy `status` files. Their observed-record
heartbeats remain session-owned. Codex/OpenCode pumps receive graph control explicitly, start
closed, and read control on mailbox arrival, with a 250 ms request deadline and three-second permit.
They renew the permit once per second only while native mail is waiting or a predecessor's hold
needs adoption. An empty mailbox closes the gate and makes no delivery-hold requests. The next
mailbox event obtains fresh control; idle time cannot leave a permit open or delay delivery.
An unavailable, malformed, or mismatched response closes the gate; a stalled driver loop also
loses its permit. Holds block new handoffs while providers, observations, and pending receipt
reconciliation continue. Input already handed to a harness cannot be recalled.

A replacement adopting an older Codex/OpenCode provider imports a still-fresh DND once with its
original expiry. The store admits that import only when no graph hold decision exists, inside
the writer transaction, so an explicit graph release cannot be overwritten by adoption. Fresh
launches ignore historical status files. Old wrapper images retain their existing behavior
until replacement/restart; the separately maintained catalog product keeps its own status/DND
transport. Deploy the matching daemon API with the new driver binary: a new driver talking to
an older daemon keeps the provider running and holds new Codex/OpenCode handoffs until that API
is available. This change does not replace mailbox or harness-observation transport.

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
| `local_blob_uploads` | Local upload ledger | Who uploaded each attachment file; quota and early read access only |
| `local_blobs` | Local upload staging | Bytes awaiting a durable claim reference; promotion into `blobs` commits with that claim |
| `operations` | Projection | Claim `_operation` metadata |
| `documents` | Projection | `doc.bound` claims and blobs |
| `desired` | Projection | Selected `intent.desired` heads |
| `events` | Read-only view | Claim payloads joined through the local eligibility index |
| `event_positions` | Local admission membership index | Effective-at-acceptance positions and indexed subject routing; stores no claim payload |
| `local_event_payloads` | Temporary local migration source | Legacy duplicate payloads, drained in bounded schema-17 migration transactions |
| `resource_observations` | Shared projection | Latest canonical `resource.observed` claim per resource subject; indexed by resource kind and opener |
| `local_resource_projection_pending` | Local projection work queue | Resource subjects whose admitted observations changed in the current transaction; flushed before commit |
| `mission_revisions` | Projection | `mission.published` claims |
| `mission_definitions` | Projection | Selected `mission.published` heads |
| `mission_runs` | Projection | `mission-run.*` claims |
| `run_generations` | Projection | `run-generation.*` claims |
| `step_runs` | Projection | `step-run.*` and `work.*` claims |
| `local_work_lease_renewals` | Local operational fact | Recent quiet lease renewals; replayed over replicated claim projections and bounded by periodic `work.renewed` anchors |
| `local_observations` | Local observations and current feed | Local evidence and numeric usage retain their existing policy; current values retain only their newest feed row |
| `local_latest_slots` | Numeric accounting cadence | Last published numeric usage and pending stop flush; categorical values and context occupancy use registers |
| `latest_values` | Current value | Owner/source time and revision, current body and newest local feed position; never part of graph inventory |
| `latest_readiness` | Current readiness | Sticky readiness for the one current incarnation; replaced on incarnation change |
| `local_usage_seen` | Local deduplication index | Stable provider response IDs from local timeline observations; never replicated |
| `local_usage_totals` | Local cumulative observation projection | Token buckets from accepted local response observations; never replicated and retained across log trimming |
| `local_seat_accounts` | Local operational fact | The account chosen for each pooled seat on this node; retained across restarts, never replicated |
| `revision_proposals` | Projection | `revision-proposal.*` claims |
| `planning_sessions` | Projection | `planning-session.*` claims |
| `planning_candidates` | Projection | `planning-session.candidate-submitted` claims |
| `planning_previews` | Projection | `planning-session.previewed` claims |
| `idempotency` | Local completed response receipts | Hashed caller keys and derived responses; new receipts expire after seven days in bounded rowid order, active work extends retention, and the fixed legacy cohort remains until the deployment-plus-30-day follow-up |
| `mission_run_requests` | Opaque validation cache | Hashed caller keys and request digests |
| `capabilities` | Local short-lived authority | Dedicated API issuance; capabilities do not replicate |
| `replica_envelopes` | Replicated authority | Authenticated outer envelopes and their exact payloads |
| `replica_records` | Admission state | Envelope records, validation results, and repair references |
| `projection_health` | Local diagnostic projection | Projection attempts against admitted authority |
| `projection_digest_repaired_claims` | Local repair exclusion cache | Known original claim identities from replicated repairs; survives receipt cleanup so retained invalid originals cannot rejoin shared source or operation projections |
| `replica_envelope_signatures` | Replicated authority | Writers' member-key signatures over envelopes, verified at receipt |
| `replica_envelope_holds` | Admission state | Envelopes held as `unsigned` or `fenced` by fleet membership; retried on each wake |
| `replication_peers` | Local transport state | Last signed exchange or transport failure for each configured peer |
| `peer_cursors` | Legacy test state | The removed cursor protocol; production does not use this table |
| `peer_replica_cursors` | Legacy test state | The removed cursor protocol; production does not use this table |

The store rebuilds operation and planning projections when it opens.
The resource observation projection is backfilled once on the first open with its projection
version and rebuilt from admitted claims during graph replay. Updates, out-of-order replication,
repair, and retained-claim deletion refresh only affected resource subjects. Neither the
projection nor its pending-subject queue is independent authority.

Replication receipt decodes the wire's base64 representation and stores an envelope before
admission interprets its CBOR payload. Malformed base64 remains exact TEXT evidence for admission.

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

`workspace.observed` is a replaceable current observation written by the owning host during
agent workspace reconciliation. It carries `host`, `workspace`, and an optional `repository`.
The reconciler writes only changed values. Repository suggestions combine those observations
with current checkout declarations; a gateway reads graph evidence and never scans host disks.
