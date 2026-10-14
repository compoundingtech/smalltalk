# st data authority

This document classifies the claim store's SQLite tables. `every_persistent_table_has_a_projection_scope`
(`crates/st3-daemon/src/store/canonical_audit.rs`) fails when a table has no class; shared projection tables
are listed in `SHARED_TABLES` there and in the production projection digests. Replicated storage tables
have their own logical authority digests; local physical fields and caches are outside those identities.

The claim log and immutable blobs are the durable graph authority.

A claim kind of `local` retention (`st schema show KIND`) is an observation that only the node
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

A node can also send every local observation to an OpenTelemetry collector. The exporter is off
unless the config names a collector:

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

Claude and Codex response token buckets are `harness.timeline` usage observations. They retain
the turn ID, model, agent, mission run, and step on the producing node. `local_usage_responses` remembers
each counted provider response, so replay across harness incarnations does not count it again;
`local_usage_spend` keeps cumulative buckets per agent incarnation, model, account, mission run and
step, and host even after the observation log is trimmed, and `local_usage_provenance` their pricing. The node replicates
`harness.usage` response rollups by agent, incarnation, model, mission step, and host. Fleet usage
queries subtract the last rollup before a period from the last rollup inside it.

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
controls an exporter in its hooks.

Fresh native seats publish activity, context, account limits and normalized timeline operations
through an st-owned observation outbox, `st-harness-events.sqlite`, in their explicit observation
directory. Matching producers commit a snapshot or timeline operation and its queued event in
one SQLite transaction with full synchronization. A pipe wake notifies the native driver;
it publishes the ordered events to `/v1/harness-events` and deletes only the acknowledged prefix.
Daemon outages, a lost acknowledgement and driver re-execution leave unacknowledged events for
replay. The source runtime and driver remain attached to historical events when a successor
publishes a backlog. Admission binds the Unix peer to its seat and checks the publishing runtime
inside the daemon's writer transaction. Source runtime, spool sequence, claim kind and usage
semantics identify an exact retry; changed input is rejected. Graph-derived claim fields are
persisted in the spool before the first HTTP attempt so retries preserve their original attribution.

The outbox is transport recovery state, not graph authority. The daemon retains each admitted
observation under its existing local/latest/durable policy. Producer snapshots remain local
coalescing and ownership evidence; timeline producers read only the previous matching entry and
latest status instead of parsing/replacing a retained JSON log. That auxiliary timeline history
is bounded to 4,096 operations and 2 MiB, independently of unacknowledged events. An outbox with 64 MiB of pending payload
rejects a producer transaction and reports the failure rather than dropping queued events.
An evidence deadline queues derived unknown once after fifteen minutes without fresh state;
a concurrent heartbeat supersedes that deadline. Native transcripts and session bindings retain
their existing source paths.

The outbox is enabled only when a provider launches fresh. A provider that a replacement binary
adopts keeps the polled record transport until its next restart.

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
launches ignore historical status files.

Runtime observations and harness observations remain separate claims. The status projection puts
runtime fields in `actual` and the current incarnation's harness fields in `harness`.

All other tables are indexes, projections, local capabilities, or transport recovery state.

Shared outcomes, selected source identities and durable action fences must agree when replicas
hold the same logical claims. A daemon, eval, repair or reader filtering one origin still follows
that rule. Local transport/history cursors, cache invalidation, publication cadence and
authorization secrets do not choose shared winners.

A seat queue has no table. Each read derives the order from the `step_runs` projection and the
replicated `agent.queue.moved` claims on the agent subject, so every replica computes the same
order from the same admitted claims.

| Table | Scope | Role and recovery source |
|---|---|---|
| `account_limit_readings` | Local | Account quota readings for this host; never replicated. |
| `account_limit_seats` | Local | Local assignment of seats to account quota windows. |
| `agent_message_days` | Local cache | Rebuildable eligible-message timestamps and daily recipient counts, plus a deduplicated queue of changed messages, over this node's retained send claims. They accelerate the daily usage estimate, are not replicated, and do not add shared identities to projection digests. |
| `agent_message_sends` | Local cache | Rebuildable eligible-message timestamps and daily recipient counts, plus a deduplicated queue of changed messages, over this node's retained send claims. They accelerate the daily usage estimate, are not replicated, and do not add shared identities to projection digests. |
| `arrangement_registers` | Shared projection | Canonical arrangement register operations derived from admitted claims. |
| `arrangements` | Shared projection | Selected canonical arrangement declarations and state; changed_index is a local arrival cursor. |
| `batches` | Shared storage, with stated local fields | Shared log metadata: origin/sequence/hash/accepted time; primary ordering inputs. Storage rather than an outcome projection. |
| `blobs` | Shared content authority | Immutable bytes/hash/size carried by durable claims or admitted blob records; included in shared projection digests. |
| `capabilities` | Local | Local secret-bearing authorization tokens, expiry/use bookkeeping. |
| `checkpoint_capture_epoch` | Local | Capture invalidation epoch, monotonic envelope/time guard bounds and trigger-migration version. Store-local physical rowid frontiers and capture timing are not replicated projection outcomes and are outside shared digests. |
| `checkpoint_claims` | Shared storage, with stated local fields | Shared immutable dropped-claim/evidence/ancestry/operation identity. Digest logical live/tombstoned identity where relevant; nodes may have trimmed at different times. |
| `checkpoint_envelopes` | Shared storage, with stated local fields | Shared immutable dropped-envelope identity. Live and trimmed nodes represent the same inventory using envelopes versus tombstones, so digest their logical union rather than treating trim progress as divergence. |
| `checkpoints` | Local | Local seal/verification/trim/adoption ledger and rowid high water; nodes legitimately occupy different protocol stages. Shared protocol facts are checkpoint.* claims. |
| `claim_signatures` | Shared storage | Each claim's signature exactly as its envelope payload carried it; written once at sealing or admission, never rewritten. Covered by the envelope hash and writer signature, so it adds nothing to any digest. |
| `claim_verdict_fresh` | Local cache | Signature verdicts folded from admitted claims in canonical order; never synced, outside every digest, rebuilt by `recheck_claim_verdicts`. |
| `claim_verdict_links` | Local cache | Signature verdicts folded from admitted claims in canonical order; never synced, outside every digest, rebuilt by `recheck_claim_verdicts`. |
| `claim_verdict_queue` | Local cache | Signature verdicts folded from admitted claims in canonical order; never synced, outside every digest, rebuilt by `recheck_claim_verdicts`. |
| `claim_verdicts` | Local cache | Signature verdicts folded from admitted claims in canonical order; never synced, outside every digest, rebuilt by `recheck_claim_verdicts`. |
| `claims` | Shared storage, with stated local fields | Shared immutable logical claim bytes/id/subject/kind/actor/predecessors/time. store_index alone is local; system-local/legacy migrated claims must be excluded from logical replicated-source digests. |
| `coordination_sends` | Local cache | Writer-maintained count and held metadata, bounded historical cursor/ceiling/progress. No new conversation payload; excluded from shared projection digests. |
| `custom_dependencies` | Shared projection | Reducer dependency relationships derived from registered custom subjects. |
| `custom_registrations` | Shared projection | Canonical custom-subject registrations. |
| `custom_sources` | Shared projection | Canonical source claims for custom-subject reducers. |
| `desired` | Shared projection | Shared selected declaration, ancestry/conflicts, ownership and full canonical body; existing ancestry/revision/id selection is deterministic and must be preserved. |
| `documents` | Shared projection | Shared immutable name/hash binding, binding_claim_id and materialized canonical binding_key. created_index is a local arrival cursor; latest selection and history order use the indexed binding_key. |
| `event_positions` | Local | Payload-free admitted event positions and subject routing; replay/admission history can differ. |
| `events` | Local view | Canonical claim payloads joined to local admitted membership by arrival index. |
| `expected_claim_signatures` | Local | A device's signature for the claim this node is about to write, moved into `claim_signatures` by the write's own transaction and gone once it returns. |
| `fleet_generation` | Local | Membership-fold invalidation counter. |
| `fleet_invite_tokens` | Local | Local invite secret/redemption attempt/authorization bookkeeping; fleet.invite-* lifecycle claims are shared. |
| `glass_heads` | Shared projection | Canonical glass declaration heads derived from admitted claims. |
| `graph_generation` | Local | Local cache invalidation counter; same graph can have different mutation counts. |
| `held_keys` | Local | Which principal each key this node holds belongs to, and its delegation chain; the private keys are files in `STATE/keys`. |
| `idempotency` | Local | Hashed caller keys and derived responses. New receipts expire after seven days in bounded rowid order; active work extends retention. Rows at or below meta.idempotency_legacy_rowid have no recorded expiry and remain. |
| `local_agent_message_pending` | Local cache | Rebuildable eligible-message timestamps and daily recipient counts, plus a deduplicated queue of changed messages, over this node's retained send claims. They accelerate the daily usage estimate, are not replicated, and do not add shared identities to projection digests. |
| `local_blob_uploads` | Local | Who uploaded an attachment file, for the per-actor quota and for who may read it before a message names it. Never replicated; the bytes are files under `<state_dir>/blobs`, see [Attachments](attachments.md). |
| `local_blobs` | Local | Staged upload bytes awaiting a durable claim reference. Promotion into `blobs` commits with the referencing claim. |
| `local_coordination_backfill` | Local cache | Writer-maintained count and held metadata, bounded historical cursor/ceiling/progress. No new conversation payload; excluded from shared projection digests. |
| `local_custom_dirty` | Local | Custom projection subjects requiring maintenance in the current transaction. |
| `local_directive_note_dirty` | Local | Person directive-note subjects whose published read needs refresh. |
| `local_directive_note_pending` | Local | Person directive-note projection work queue. |
| `local_event_payloads` | Temporary local | Present only while duplicate local event payloads drain in bounded migration transactions. |
| `local_git_change_latest` | Local | Current workspace Git observations on this node. |
| `local_git_change_records` | Local | Local Git change observation records and cursors. |
| `local_glass_head_dirty` | Local | Glass heads whose published read needs refresh. |
| `local_glass_head_pending` | Local | Glass-head projection work queue. |
| `local_idle_messages` | Local | Disposable message delivery and source-wake index, maintained by claim writes and backfilled in bounded startup batches. Excluded from replicated projection digests. |
| `local_latest_slots` | Local | Local latest-retention publication slots and pending local observation pointers. |
| `local_limit_stops` | Local | Seats this node's limits policy stopped, once per account and weekly window. |
| `local_mailbox_argv_bindings` | Local | Launch-argv binding evidence for native mailbox ownership. |
| `local_mailbox_bindings` | Local | Stable binding request tokens mapped to daemon-allocated epochs. Lost acknowledgements retry the same binding; retired tokens cannot allocate a successor. Excluded from replicated projection digests. |
| `local_mailbox_leases` | Local | Authenticated native custody for each subject/component, including exact runtime/provider/session/sequence, process birth, token/epoch and revocation. Retained across daemon restart; excluded from replicated projection digests. |
| `local_mailbox_owners` | Local | This daemon's native socket subscription owner, live runtime incarnation and replacement epoch. Survives daemon restart; excluded from replicated projection digests. |
| `local_observations` | Local | Local-retention observations and their local frontier/id; never replicated. |
| `local_publication_claims`, `local_publication_starts`, `local_publication_pending`, `local_publication_batches`, `local_publication_bootstrap`, `local_publication_launch_epoch` | Local derived state | Rebuildable retained runtime and incarnation launch evidence, canonical keys, exact delta and coalesced batch queues, cache generation, and durable bootstrap cursor. Outside replicated digests; missing or incomplete evidence is Unknown. |
| `local_resource_projection_pending` | Local projection work queue | Resource subjects whose admitted observations changed in the current transaction; flushed before commit |
| `local_seat_accounts` | Local operational fact | The account chosen for each pooled seat on this node; retained across restarts, never replicated |
| `local_subscription_mission_deferrals` | Local | Local reconciler capacity backoff/retry scheduling. |
| `local_stop_deferral_backfill` | Local cache | Trigger-maintained claim ids, subjects and recovered-failure references of retained-stop diagnostics, faults and recoveries, plus the ceiling and completion of the one-time copy of older claims. Rebuildable from claims; never replicated; excluded from shared projection digests. |
| `local_usage_provenance` | Local | Pricing contributions for the local spend slots. |
| `local_usage_responses` | Local | Counted provider response IDs and times; deduplicates replay across harness incarnations. |
| `local_usage_spend` | Local | Cumulative token and cost buckets; retained across observation-log trimming. |
| `local_work_lease_renewals` | Local | Local lease extensions, re-applied after canonical replay. Durable replicated lease anchors remain shared. |
| `message_index` | Shared projection | Shared message membership/closed fact. created_index is a local cache cursor. Subject order/onset/lifecycle must come from canonical message claims, never this cursor. |
| `meta` | Local | Local schema/version/writer/cache/frontier bookkeeping; individual fleet-binding metadata copies do not turn the table into a shared projection. |
| `mission_definitions` | Shared projection | Shared selected mission revision/state/claim. |
| `mission_revisions` | Shared projection | Shared full immutable mission revision/state/body and binding claim. created_index is a local historical cursor; repeated identical revision bindings need a deterministic representative claim. |
| `mission_run_after` | Shared projection | Shared predecessor run dependency. |
| `mission_run_deadlines` | Shared projection | Shared durable run timeout/deadline derived from run creation; evaluate expiration with an explicit as_of. |
| `mission_run_requests` | Local | Local original request-hash cache paired with idempotency; run identities in the request API are origin-scoped. |
| `mission_runs` | Shared projection | Shared run creation inputs, hierarchy, ownership, workspace, requester, mode, status/phase and durable timestamps. Digest full logical row, not just status columns. |
| `operations` | Shared projection | Shared operation identity, canonical claim, request digest and conflict state, derived from live claims plus tombstones. After trim, operations with no live claim are served by tombstones. |
| `peer_cursors` | Local | Local per-peer legacy transport progress. |
| `peer_replica_cursors` | Local | Local per-peer/per-writer transport progress. |
| `person_directive_notes` | Shared projection | Current person-authored directive notes derived from claims. |
| `planning_candidates` | Shared projection | Shared immutable variant/revision/document references, mission revision and submission timestamp. |
| `planning_previews` | Shared projection | Serialized preview claim fields and candidate fence. Embedded store_index is claim payload, not this replica’s arrival cursor. |
| `planning_sessions` | Shared projection | Shared launch/planner/request/config, status, selected revision, ownership and durable timestamps. |
| `projection_digest_generation` | Local | Projection-digest cache invalidation generation. |
| `projection_digest_operation_rows` | Local | Maintained operation-row digest contributions. |
| `projection_digest_repaired_claims` | Local | Repair exclusion cache over known original claim identities. Repair meaning and replacements are covered by shared claims; raw originals remain authenticated envelope history and do not contribute to source, operation or actual-state projections. Actual values, provenance, runtime origin and runtime-epoch reads use this durable repaired-ID membership even after local receipt cleanup. |
| `projection_digest_state` | Local | Maintained shared projection digest state; the logical digest is shared, these rows are local maintenance. |
| `projection_health` | Local | Local admission/projection/quarantine status, frontier, errors and observation timestamps. Deterministic shared degraded-state summaries must be separate from receipt health. |
| `replica_envelope_holds` | Local | Local validation/fencing hold state and observation time. |
| `replica_envelope_signatures` | Shared storage, with stated local fields | Shared signed envelope identity/member-key/signature; stored_at is local receipt time. Authority/signature inventory is distinct from outcome projection digests. |
| `replica_envelopes` | Shared storage, with stated local fields | Shared authenticated writer/sequence/hash/payload identity; relay, receipt_state/errors and received_at are local. Inventory hashes immutable envelope identity, not physical rows. |
| `replica_records` | Shared storage, with stated local fields | Shared raw record/position/claim identity and replicated repair meaning; admission status/errors/updated_at are local. Normalize from retained records plus tombstones; never compare physical receipt rows. |
| `replication_peers` | Local | Local connectivity, peer digest observations, receive timings and errors. |
| `replication_refusals` | Local | Outbound Fabric grant refusals and observation timestamps; route policy state outside shared projections. |
| `resource_observations` | Shared projection | Latest canonical `resource.observed` claim per resource subject; indexed by resource kind and opener |
| `revision_proposals` | Shared projection | Shared candidate/source generation, reviewers/approvals, cutover/compatibility, status/preview/successor and durable timestamps. |
| `run_generations` | Shared projection | Shared generation identity, revision, predecessor, actor/reason, status and durable timestamps. |
| `step_runs` | Shared projection | Step definition, attempts, results, durable claimant/incarnation and readiness. Local lease_expires_at_unix_ms and updated_at_unix_ms overlays are excluded from projection digests. |
| `stop_deferral_claims` | Local cache | Trigger-maintained claim ids, subjects and recovered-failure references of retained-stop diagnostics, faults and recoveries, plus the ceiling and completion of the one-time copy of older claims. Rebuildable from claims; never replicated; excluded from shared projection digests. |
| `unread_mail` | Local cache | Rebuildable unread-message timestamp and prefix-count indexes, plus a deduplicated queue of changed messages, over this node's retained sent/read/closed claims. They accelerate current age counts, are not replicated, and do not add shared identities to projection digests. |
| `unread_mail_pending` | Local cache | Rebuildable unread-message timestamp and prefix-count indexes, plus a deduplicated queue of changed messages, over this node's retained sent/read/closed claims. They accelerate current age counts, are not replicated, and do not add shared identities to projection digests. |
| `unread_mail_prefixes` | Local cache | Rebuildable unread-message timestamp and prefix-count indexes, plus a deduplicated queue of changed messages, over this node's retained sent/read/closed claims. They accelerate current age counts, are not replicated, and do not add shared identities to projection digests. |

Temporary clock simulation and checkpoint proof/adoption scratch state is local.
SQLite `sqlite_sequence` is a local allocation counter. Rust read caches and exported
mailbox files are disposable local caches; their shared source selections still
follow the canonical rules above.

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

`workspace.observed` is a latest-only replicated observation written by the owning host during
agent workspace reconciliation. It carries `host`, `workspace`, and an optional `repository`.
The reconciler writes only changed values. Repository suggestions combine those observations
with current checkout declarations; a gateway reads graph evidence and never scans host disks.
