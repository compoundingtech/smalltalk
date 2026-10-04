# Canonical shared projections audit

Audited 2026-09-30 against public main commit `37207bc477b088298c06795a2ea7636791500b2a` (schema version 13). This is the audit step, before fixes. References and fixture names are invented. The audit covers all 42 persistent schema tables, the 16 shared materialized logical tables, every literal arrival-order clause in store.rs, and shared readers/folds outside tables.

## Finding

An identical current graph_digest proves equality only of selected columns from six tables, not equality of shared outcomes. GRAPH_DIGEST_TABLES includes desired, mission_definitions, mission_runs, run_generations, step_runs and revision_proposals; four of those also omit meaningful columns. Planning, documents, message state, gates, subscriptions, schedules, fault episodes, queue state and other claim-derived answers can disagree while that digest matches.

The baseline has 82 production lines containing ORDER BY and store_index on the same line (including the two clauses in events_tail_bounded), plus the multiline seat_queue_inputs_tx order. Three more such lines are isolated test SQL. The earlier estimate of 72 was not exhaustive. The inventory below records all 86 matching source lines/order starts; CANONICAL_ORDER/descending are separately covered because their declarations span lines rather than literal ORDER BY statements.

## Shared versus local rule

Shared means an outcome, selected source identity, serialized result or durable action fence that replicas holding the same logical claims must agree on. A caller being a daemon, an eval, a repair, or filtering one origin is not an exemption. Local means a transport/event/history cursor, cache invalidation frontier, local-retention observation, publication cadence, authorization secret, or explicitly host-owned lease/retry/live overlay. A local frontier can restrict the set of claims a historical reader sees, but the shared fold of that set still uses canonical order.

Use one helper for shared SQL and in-memory folds with the stable key: numeric accepted time, batch writer, replica sequence, batch identity, immutable wire record position, final claim identity. Preserve causality, explicit episode/generation/attempt references and existing deterministic ancestry/revision/id conflict rules. Do not replace commutative desired/operation resolution with arbitrary last-writer-wins. CANONICAL_ORDER and CANONICAL_ORDER_DESC currently end in claims.store_index; that is stable only as relative position inside one atomically inserted batch. Full graph replay already reads replica_records.position with a legacy fallback. Unify that logic and use the same key for subset/incremental replay, not only the full replay. The helper API is an implementation decision for the fix step; attention-builder will reuse it.

A source guard must distinguish shared reduction from local cursor scans and intra-batch wire-position reconstruction. It must cover the literal inventory, helper interpolations, in-memory ties and transitive callers of raw paginated history. A regex that bans every store_index would break authorized local cursors, while a regex limited to rebuild_* would miss most problems.

## Persistent table inventory

Mixed storage tables below are classified by their logical shared fields; local physical columns are named explicitly. No unclassified persistent table remains.

| Table | Scope | Reason / digest boundary |
| --- | --- | --- |
| `meta` | Local | Local schema/version/writer/cache/frontier bookkeeping; individual fleet-binding metadata copies do not turn the table into a shared projection. |
| `batches` | Shared storage, with stated local fields | Shared log metadata: origin/sequence/hash/accepted time; primary ordering inputs. Storage rather than an outcome projection. |
| `claims` | Shared storage, with stated local fields | Shared immutable logical claim bytes/id/subject/kind/actor/predecessors/time. store_index alone is local; system-local/legacy migrated claims must be excluded from logical replicated-source digests. |
| `operations` | Shared projection | Shared operation identity, canonical claim, request digest and conflict state, derived from live claims plus tombstones. After trim, operations with no live claim are served by tombstones. |
| `blobs` | Shared projection | Shared immutable bytes/hash/size carried by durable claims or admitted blob records; all columns remain digested, including content retained after checkpoint trimming. |
| `local_blob_uploads` | Local | Who uploaded an attachment file, for the per-actor quota and for who may read it before a message names it. Never replicated; the bytes are files under `<state_dir>/blobs`, see [Attachments](attachments.md). |
| `local_blobs` | Local | Staged upload bytes awaiting a durable claim reference. Promotion into `blobs` commits with the referencing claim. |
| `documents` | Shared projection | Shared immutable name/hash binding, binding_claim_id and materialized canonical binding_key. created_index is a local arrival cursor; latest selection and history order use the indexed binding_key. |
| `desired` | Shared projection | Shared selected declaration, ancestry/conflicts, ownership and full canonical body; existing ancestry/revision/id selection is deterministic and must be preserved. |
| `idempotency` | Local | Local opaque HTTP/operation response cache with local indexes; shared operation identity is operations plus checkpoint_claims. |
| `mission_run_requests` | Local | Local original request-hash cache paired with idempotency; run identities in this legacy API are origin-scoped. |
| `events` | Local | Local stream notifications keyed by arrival index; replay/admission history can differ. |
| `message_index` | Shared projection | Shared message membership/closed fact. created_index is a local cache cursor. Subject order/onset/lifecycle must come from canonical message claims, never this cursor. |
| `peer_cursors` | Local | Local per-peer legacy transport progress. |
| `peer_replica_cursors` | Local | Local per-peer/per-writer transport progress. |
| `replica_envelopes` | Shared storage, with stated local fields | Shared authenticated writer/sequence/hash/payload identity; relay, receipt_state/errors and received_at are local. Inventory hashes immutable envelope identity, not physical rows. |
| `replica_records` | Shared storage, with stated local fields | Shared raw record/position/claim identity and replicated repair meaning; admission status/errors/updated_at are local. Normalize from retained records plus tombstones; never compare physical receipt rows. |
| `projection_health` | Local | Local admission/projection/quarantine status, frontier, errors and observation timestamps. Deterministic shared degraded-state summaries must be separate from receipt health. |
| `replication_peers` | Local | Local connectivity, peer digest observations, receive timings and errors. |
| `replication_refusals` | Local | Added after this audit: direct outbound Fabric grant refusals and observation timestamps. Route policy cache, never a replicated outcome. |
| `capabilities` | Local | Local secret-bearing authorization tokens, expiry/use bookkeeping. |
| `mission_revisions` | Shared projection | Shared full immutable mission revision/state/body and binding claim. created_index is a local historical cursor; repeated identical revision bindings need a deterministic representative claim. |
| `mission_definitions` | Shared projection | Shared selected mission revision/state/claim. |
| `mission_runs` | Shared projection | Shared run creation inputs, hierarchy, ownership, workspace, requester, mode, status/phase and durable timestamps. Digest full logical row, not just status columns. |
| `mission_run_deadlines` | Shared projection | Shared durable run timeout/deadline derived from run creation; evaluate expiration with an explicit as_of. |
| `mission_run_after` | Shared projection | Shared predecessor run dependency. |
| `run_generations` | Shared projection | Shared generation identity, revision, predecessor, actor/reason, status and durable timestamps. |
| `step_runs` | Shared projection | Shared step definition/selector, attempt, worker result, durable claimant/incarnation, readiness/blocking and ownership. lease_expires_at_unix_ms and updated_at_unix_ms currently receive local renewal overlays: split or recover their replicated anchors for hashing; do not hash the effective local overlay. |
| `local_work_lease_renewals` | Local | Local lease extensions, re-applied after canonical replay. Durable replicated lease anchors remain shared. |
| `local_mailbox_owners` | Local | This daemon's native socket subscription owner, live runtime incarnation and replacement epoch. Survives daemon restart; excluded from replicated projection digests. |
| `local_mailbox_bindings` | Local | Stable binding request tokens mapped to daemon-allocated epochs. Lost acknowledgements retry the same binding; retired tokens cannot allocate a successor. Excluded from replicated projection digests. |
| `local_observations` | Local | Local-retention observations and their local frontier/id; never replicated. |
| `local_subscription_mission_deferrals` | Local | Local reconciler capacity backoff/retry scheduling. |
| `local_usage_spend` | Local | Local provider usage and cost accumulation before publication. |
| `local_usage_responses` | Local | Local usage-input deduplication, trimmed after 30 days. |
| `local_limit_stops` | Local | Seats this node's limits policy stopped, once per account and weekly window. |
| `local_latest_slots` | Local | Local latest-retention publication slots and pending local observation pointers. |
| `revision_proposals` | Shared projection | Shared candidate/source generation, reviewers/approvals, cutover/compatibility, status/preview/successor and durable timestamps. |
| `planning_sessions` | Shared projection | Shared launch/planner/request/config, status, selected revision, ownership and durable timestamps. |
| `planning_candidates` | Shared projection | Shared immutable variant/revision/document references, mission revision and submission timestamp. |
| `planning_previews` | Shared projection | Shared serialized preview claim fields, graph/diff/response/hash and candidate fence. store_index here is an originating claim payload field, not this replica’s admission cursor: identical claim payloads must compare identically. Future cross-node preview fencing should use stable claim/frontier identity. |
| `graph_generation` | Local | Local cache invalidation counter; same graph can have different mutation counts. |
| `projection_digest_repaired_claims` | Local | Repair exclusion cache over known original claim identities. Repair meaning and replacements are covered by shared claims; raw originals remain authenticated envelope history and do not contribute to source or operation projections. |
| `replica_envelope_signatures` | Shared storage, with stated local fields | Shared signed envelope identity/member-key/signature; stored_at is local receipt time. Authority/signature inventory is distinct from outcome projection digests. |
| `claim_signatures` | Shared storage | Each claim's signature exactly as its envelope payload carried it; written once at sealing or admission, never rewritten. Covered by the envelope hash and writer signature, so it adds nothing to any digest. |
| `claim_verdicts`, `claim_verdict_links`, `claim_verdict_queue`, `claim_verdict_fresh` | Local cache | Signature verdicts folded from admitted claims in canonical order; never synced, outside every digest, rebuilt by `recheck_claim_verdicts`. |
| `expected_claim_signatures` | Local | A device's signature for the claim this node is about to write, moved into `claim_signatures` by the write's own transaction and gone once it returns. |
| `held_keys` | Local | Which principal each key this node holds belongs to, and its delegation chain; the private keys are files in `STATE/keys`. |
| `fleet_invite_tokens` | Local | Local invite secret/redemption attempt/authorization bookkeeping; fleet.invite-* lifecycle claims are shared. |
| `replica_envelope_holds` | Local | Local validation/fencing hold state and observation time. |
| `checkpoint_envelopes` | Shared storage, with stated local fields | Shared immutable dropped-envelope identity. Live and trimmed nodes represent the same inventory using envelopes versus tombstones, so digest their logical union rather than treating trim progress as divergence. |
| `checkpoint_claims` | Shared storage, with stated local fields | Shared immutable dropped-claim/evidence/ancestry/operation identity. Digest logical live/tombstoned identity where relevant; nodes may have trimmed at different times. |
| `checkpoints` | Local | Local seal/verification/trim/adoption ledger and rowid high water; nodes legitimately occupy different protocol stages. Shared protocol facts are checkpoint.* claims. |

The temporary tables write_clock, sealed_claims, sealed_envelopes, canonical_index, adopted_envelopes and adopted_claims are Local: clock simulation or checkpoint proof/adoption scratch state. SQLite sqlite_sequence is a local allocation counter. There are no persistent SQL views in the audited schema. Rust actual/subject/message/status/replication snapshot caches and exported mailbox files are Local disposable caches; their shared source selections are not exempt.

### Arrangement heads added after the baseline audit

Person arrangements are shared claim-derived projections, not local sidebar caches.
`arrangements` stores subject-keyed owner/creation/retirement/revision heads with canonical
winner keys and projected update time. `arrangements_owner_index(owner, subject)` indexes
owner identity; `arrangements_live_owner_index(owner, subject)` selects created, unretired
collection rows, and `arrangements_changed_index(changed_index)` supports local change seeks.
`arrangements_owner_changed_index(owner, changed_index)` seeks each owner's stale frontier,
including retired and pending heads without traversing them.
`arrangement_registers` stores `(subject, register)`-keyed raw
values, winning claim revisions and canonical winner keys. Admission updates these heads
in the same transaction as the durable `arrangement.edited` claim. List/detail and write
validation read heads without folding edit history.

Both tables are graph-digest-covered. The local `arrangements.changed_index` invalidation
frontier is excluded; owner, retirement, source revisions, raw register values and canonical
winner keys are shared. Replay rebuilds both tables, and checkpoint proof readers compare
the same arrangement read answers. There is no new checkpoint drop rule, including for
agent-authored edits. Effective tombstone ancestors and concurrent-cycle cuts derive from
raw position heads deterministically without rewriting their registers.


## Every store_index order

Locations below refer to the audited commit, so later line-number changes do not invalidate the inventory. Shared rows must move to the canonical helper, including commutative enumerations whose returned ordering is observable. Local rows may retain arrival order only for the purpose stated.

Added operational reader `outcome_history` is a local raw terminal-claim history page, using
the serving node's claim index as its continuation cursor. It does not reduce shared state.
Its recovered reasons and `mission_overview` reasons use canonical claim ordering; a local
history cursor is not an exception for choosing a shared reason.

| store.rs line | Function / constant | Scope | Reason |
| --- | --- | --- | --- |
| 810 | `AGENT_STATUS_INDEX_QUERY` | Local | Local snapshot/cache invalidation frontier; this index names what this node has seen. |
| 2582 | `authoring_pull_request_runs_tx` | Shared | Shared association between an observed PR and authoring runs; first-seen deduplication affects returned order. |
| 2659 | `claims_page_query` | Local | Local claim-log pagination with a node-specific cursor; shared reducers must not consume this arrival-ordered page as a fold. |
| 2825 | `agent_projection_index` | Local | Local status-cache invalidation frontier; newest arrival relevant to the cache. |
| 5750 | `open_reconcile_faults` | Shared | Shared fault/recovery facts folded by subject/scope even when filtered to one origin; origin filtering does not make replicated facts local. |
| 5792 | `open_reconcile_fault_claim` | Shared | Shared selected fault/recovery claim for a subject/scope/origin. |
| 5809 | `reconcile_fault` | Shared | Shared selected fault/recovery reason used by status and attention. |
| 5933 | `mission_run_origin` | Shared | Shared creator/ownership attribution from the selected run-creation claim. |
| 7095 | `work_action` | Local | Local lease-publication cadence; quiet renewals are host-owned. Durable work state and lease anchors remain shared. |
| 9363 | `apply_operational_repair` | Shared | Shared durable repair receipt/winner and idempotent evidence; selection must be stable. |
| 10057 | `events_after_bounded` | Local | Local event-stream cursor; events are notifications about admission on this node. |
| 10077 | `events_tail_bounded` | Local | Local event-stream tail and presentation order. |
| 10078 | `events_tail_bounded` | Local | Local event-stream tail and presentation order. |
| 10123 | `projection_time_at` | Local | Local snapshot frontier timestamp. Do not use it as a shared winner or a cross-node timeless digest input. |
| 10142 | `events_after_filtered` | Local | Local event-stream cursor, including filtered notifications. |
| 10445 | `eval_runtime_records` | Shared | Shared desired/gate facts exported for evals; local test execution does not make desired selection shared-arrival-safe. |
| 10483 | `eval_runtime_records` | Shared | Shared desired/gate facts exported for evals; local test execution does not make desired selection shared-arrival-safe. |
| 10763 | `member_reconcile_fault` | Local | Local runtime.reconcile-decision observation at a local snapshot. |
| 10796 | `member_reconcile_faults_for` | Local | Local runtime.reconcile-decision observation at a local snapshot. |
| 10889 | `pending_observer_refresh_attempt` | Shared | Shared pending refresh selection; LIMIT 1 must select a canonical request. |
| 10905 | `gate_request_for_owner` | Shared | Shared current gate request selection and approval fences. |
| 11221 | `pending_attention_closed_by_condition` | Shared | Shared historical attention source/condition selection; derived-attention replaces current heuristics, but retained historical readers still need canonical order. |
| 11318 | `attention_requests` | Shared | Shared attention history listing; preserve stable source order. |
| 11350 | `pending_attention_requests_raised_by` | Shared | Shared pending attention source set, even for host-authored requests. |
| 11502 | `attention_items` | Shared | Shared attention source selection/onset time and subscription-failure fold; acceptance time alone is not a total order. |
| 11533 | `attention_items` | Shared | Shared attention source selection/onset time and subscription-failure fold; acceptance time alone is not a total order. |
| 11697 | `selected_actual_origin` | Shared | Shared selected runtime/resource ownership attribution; must match canonical actual-state selection. |
| 11712 | `selected_actual_origin` | Shared | Shared selected runtime/resource ownership attribution; must match canonical actual-state selection. |
| 12396 | `pending_subscription_mission_requests` | Shared | Shared durable pending delivery requests; affects dispatch order and grouped attention sources. |
| 12523 | `subscription_mission_deferral` | Shared | Shared legacy replicated deferral; distinct from local_subscription_mission_deferrals. |
| 12568 | `pending_schedule_work_requests` | Shared | Shared pending schedule requests and action ordering. |
| 12588 | `schedule_work_start_for_request` | Shared | Shared request disposition/start winner. |
| 12627 | `usage_summary_at` | Shared | Shared replicated usage fold: response_rollup and context occupancy are order-sensitive. |
| 12661 | `usage_summaries_at` | Shared | Shared usage fold per subject; same issue as usage_summary_at. |
| 12921 | `usage_period_rows` | Shared | Shared period totals; equal observed-time ties currently use local store index. |
| 13051 | `claims_for_subject_kind_at` | Local | Local raw-history page using a local cursor; callers reducing shared state need the canonical helper instead. |
| 13235 | `agent_last_activity_at` | Local | Local operational activity/silence view combines host-only timeline observations with newest arrivals. |
| 13241 | `agent_last_activity_at` | Local | Local operational activity/silence view combines host-only timeline observations with newest arrivals. |
| 13242 | `agent_last_activity_at` | Local | Local operational activity/silence view combines host-only timeline observations with newest arrivals. |
| 13243 | `agent_last_activity_at` | Local | Local operational activity/silence view combines host-only timeline observations with newest arrivals. |
| 13301 | `timeline_claim_rows_for_incarnation_at` | Local | Local harness timeline/history page. Current timeline retention is local; legacy rows can appear in claims. |
| 13339 | `claims_for_kind_at` | Local | Local raw-kind history page; not a permitted source of arrival-ordered shared reduction. |
| 13435 | `work_claim_is_orphaned` | Shared | Shared runtime-incarnation/terminal-state selection controls durable work ownership. |
| 15220 | `export_replication_for_heads` | Shared | Shared immutable batch position, not inter-batch arrival. Relative store indexes within one atomically admitted batch preserve its wire position; migrate to explicit position for uniform enforcement. |
| 16930 | `resolve_mission_run_inputs` | Shared | Shared resource input claim identity persisted in a run; latest must be canonical. |
| 17066 | `desired_row_at` | Shared | Shared desired selection within a local historical frontier. Winner is ancestry/revision/id and is commutative today; traversal and returned logical rows must still be canonical. |
| 17596 | `latest_resource_kind` | Shared | Shared resource schema/type selection from durable observations. |
| 18503 | `rebuild_planning_tx` | Shared | Shared planning fold; out-of-order started/candidate/approved causes skipped updates, quarantine, and different launch state. |
| 18785 | `validate_message_transition` | Shared | Shared message lifecycle validation; newest arrival can authorize/reject the wrong transition. |
| 19017 | `latest_claim_id_tx` | Shared | Shared predecessor selection for new durable claims. Evidence identity must not depend on arrival. |
| 19470 | `current_harness_at` | Shared | Shared replicated harness/work activity reduction; local host overlays are separate. Work activity still compares physical indexes against runtime/harness indexes. |
| 19525 | `claim_ids_at` | Shared | Shared evidence/optimistic-fence identity set at a local frontier; serialized ordering must be stable. |
| 19546 | `desired_conflicts_at` | Shared | Shared conflict set at a local frontier; membership is commutative, but returned ordering is observable. |
| 19825 | `pending_human_reviews_tx` | Shared | Shared human gate/review source ordering and pending state. |
| 19866 | `attention_request_view_tx` | Shared | Shared legacy ask/resolution selection; equal acceptance times lack writer/sequence/position ties. |
| 19879 | `attention_request_view_tx` | Shared | Shared legacy ask/resolution selection; equal acceptance times lack writer/sequence/position ties. |
| 19968 | `pending_attention_requests_tx` | Shared | Shared pending attention sources; physical arrival changes order. |
| 20294 | `attention_target_moved_on_tx` | Shared | Shared terminal/recovery episode selection; ClaimMoment also compares (accepted time, local index), which must become a canonical identity. |
| 20365 | `attention_target_moved_on_tx` | Shared | Shared terminal/recovery episode selection; ClaimMoment also compares (accepted time, local index), which must become a canonical identity. |
| 20382 | `attention_target_moved_on_tx` | Shared | Shared terminal/recovery episode selection; ClaimMoment also compares (accepted time, local index), which must become a canonical identity. |
| 20412 | `attention_target_moved_on_tx` | Shared | Shared terminal/recovery episode selection; ClaimMoment also compares (accepted time, local index), which must become a canonical identity. |
| 20447 | `pull_request_state_tx` | Shared | Shared selected PR resource state used to close/currently show attention. |
| 20522 | `loop_run_moved_on_tx` | Shared | Shared loop recovery/episode selection. |
| 20578 | `mission_run_ended_tx` | Shared | Shared terminal run episode onset. |
| 20683 | `attention_target_state_tx` | Shared | Shared runtime/observer/subscription/loop/message source state selection. |
| 20694 | `attention_target_state_tx` | Shared | Shared runtime/observer/subscription/loop/message source state selection. |
| 20702 | `attention_target_state_tx` | Shared | Shared runtime/observer/subscription/loop/message source state selection. |
| 20711 | `attention_target_state_tx` | Shared | Shared runtime/observer/subscription/loop/message source state selection. |
| 21180 | `operational_repair_plan_tx` | Shared | Shared persisted wake failure selection used in fenced durable repairs; local lease/time inputs stay outside timeless shared digests. |
| 23307 | `fleet_invites` | Shared | Shared invite created/redeemed/revoked lifecycle; token secrets and attempt counters are separate local rows. |
| 23922 | `seed_replica_envelopes_tx` | Shared | Shared immutable batch wire position during legacy envelope seeding; relative intra-batch index is stable, unlike global arrival. |
| 26124 | `generation_run_tx` | Shared | Shared run ownership fallback from generation/run creation claims. |
| 26139 | `generation_run_tx` | Shared | Shared run ownership fallback from generation/run creation claims. |
| 26175 | `run_tree_of_tx` | Shared | Shared aggregate/root ownership fallback; chosen creation/proposal claim must be stable. |
| 26204 | `run_tree_of_tx` | Shared | Shared aggregate/root ownership fallback; chosen creation/proposal claim must be stable. |
| 26519 | `try_project_simple_replication_tx` | Local | Local projection-backlog frontier scan. Each shared aggregate must still reduce canonically; sorting run_updates only by acceptance time is insufficient for equal-time ties. |
| 28599 | `seat_queue_inputs_tx` | Shared | Shared queue moves: nearly canonical but missing batch-id/explicit-position/final-id tie breaks; use the helper. |
| 29548 | `enrich_step_summaries_at` | Shared | Shared durable progress/submission summary at fixed as_of; equal-time local-index ties alter latest content. |
| 29846 | `enrich_step_wake_at` | Shared | Shared durable wake-failure episode selection; host-local live harness overlay remains local. |
| 29908 | `harness_incarnation_for_key_at` | Shared | Shared replicated incarnation selection for wake fences. |
| 30202 | `active_step_blockers_tx` | Shared | Shared pending blocker source ordering at fixed as_of. |
| 31216 | `loop_run_views_tx` | Shared | Shared loop state winner and ordered round results. |
| 31233 | `loop_run_views_tx` | Shared | Shared loop state winner and ordered round results. |
| 34044 | `the_newest_message_to_a_recipient_is_found_by_index` | Local | Isolated test/fixture verification query; does not project live shared state. |
| 39669 | `replication_inventory_uses_the_batch_indexes` | Local | Isolated test/fixture verification query; does not project live shared state. |
| 42599 | `a_worker_can_submit_each_retry_attempt_once` | Local | Isolated test/fixture verification query; does not project live shared state. |

CANONICAL_ORDER (705-706) and CANONICAL_ORDER_DESC (707-709): Shared ordering helpers, with the intra-batch physical-index limitation described above. RUNTIME_SUBJECTS and SUBJECTS_IN_RANGE order by subject, not by store_index; their index bounds are Local snapshot membership. Index definitions on store_index accelerate local log access but do not themselves select a shared winner.

## Other arrival-derived state that an ORDER BY store_index search misses

- Document latest binding/token selection: created_index DESC in put_document, mission resolution, latest_document_hash, document_bindings_at and latest_document_token. That cursor cannot choose the shared version. Repeated same-name/same-hash bindings also need a canonical binding claim representative rather than INSERT OR IGNORE arrival.
- Message ordering: message_index created_index, MIN(store_index), recipient mailbox order and exported filenames are local cursor metadata. The shared mailbox source, status and onset use canonical claim identity/time; compare logical message rows and recipient order independently of physical file names.
- ClaimMoment at 20229 compares (accepted time, store_index) for attention episode closure. current_harness_at compares work/runtime/harness physical indexes. usage_period_rows uses (observed time, index) and usage_summary_from_rows overwrites rollup/context fields as rows arrive. Canonical SQL alone is insufficient unless these comparisons change too.
- try_project_simple_replication_tx sorts run_updates only by accepted_at_unix_ms; equal-time ordering inherits the local scan. Its backlog scan may remain local, but shared updates must use the complete key. seat_queue_inputs_tx has an almost-canonical hand-written order that must use the same helper.
- Partial shared digests hide differences inside run/generation/step/proposal fields, even in already covered tables. updated_at from effective local lease renewal must be separated from the durable source timestamp rather than blindly hashed or silently used as a shared clock.

## Claim-derived shared readers without a materialized table

These are Shared: actual/effective subject state and selected runtime/resource owner; harness replicated snapshot; message lifecycle/unread/reminders; gate request/result/review facts; subscription and schedule request dispositions; observer refresh/state; loop state/round results; reconcile and delivery failure/recovery episodes; fleet member/invite lifecycle; seat queue movement; progress/result summaries; replicated usage snapshots; shared checkpoint protocol facts. Shared outputs at a fixed as_of/recipient need a canonical source representation and per-source/table digest coverage even when currently folded directly from claims. Local reachability, harness liveness, lease overlay and notification delivery do not define shared completion or disappearance.

Some readers already use CANONICAL_ORDER or sort IDs; retain their deterministic behavior and route their implementation through the common helper. Checkpoint proof canonicalizes physical indexes on a copy before comparison, which can hide an arrival-order bug on real replicas. Therefore the new test also compares real stores before proof normalization and after actual production tombstone/delete/replay operations.

## Derived-attention coordination

The attention builder confirmed that its reviewed proposal introduces one snapshot function, not an attention table. Design: `doc/fleet/smalltalk/attention-derived-design@e3a78d7c336f3aa068629121baa967c40e9ef7695885de62766667bc0c0975b3`; reply: `message/177021f57187a57f`, following audit coordination `message/60e73ec7b9f4374a`.

Its sources are person steps/human gates/revision approvals, launch candidate/preview review, unread person messages/reminders, held subscription source gates, current durable operational failures and checkpoint waits. Legacy attention.requested/resolved becomes historical. Proposed person asks extend step_runs and claim-derived person metadata; final DDL is pending in that mission. Hash new shared step columns automatically and add canonical source inventories for nonmaterialized dependencies. Do not hash the local checkpoints ledger as if every host had sealed/trimmed simultaneously.

The view must be tested separately at identical explicit as_of and recipients. Identity includes source + recipient + gate/episode, preserving multiple reviewers, attempt/generation replacement, original waiting age, reminder versions, preview replacement and failure/recovery episode fences. Sort by priority, waiting_since, source_id, person_id, episode. Ownership filtering precedes pagination. Final helper API and table inventory additions must be sent back when implemented. No need to wait for that mission to complete this audit.

## Regression written first

`crates/st3/src/store/canonical_audit.rs`, included under store::tests::canonical_audit, compares all 16 shared logical materialized tables with independent SHA-256 hashes of sorted full logical rows. It excludes only the explicitly local arrival/lease-overlay columns listed in SHARED_TABLES; selected document and message readers are compared separately, and replicated usage is checked. These hashes are a test oracle, not the planned incremental production digest API.

The history includes mission/work/run state, two immutable document versions, started/candidate/preview/approved planning, message send/close, cumulative usage and harness transitions. It uses equal acceptance times, reversed and interleaved single-envelope arrival, on-disk restart plus full replay, checkpoint proof, and real production tombstone/delete/replay on isolated stores. All mismatches are accumulated by phase and table instead of stopping at the first planning mismatch. No shared daemon or agent seat is restarted.

Validation command: `cargo test -p st3 --lib every_shared_projection_agrees_after_shuffle_restart_and_checkpoint -- --nocapture`.

Observed baseline failure: reverse delivery disagrees on planning_sessions, planning_candidates,
planning_previews, selected document bindings and usage summary in all three phases. Interleaved
delivery disagrees on planning_sessions, planning_candidates and selected document bindings in
all three phases. The six-table graph_digest still matches in every comparison. Both checkpoint
proofs pass, confirming that proof normalization alone does not expose the live-replica bug.

Two companion checks are included under the same test module:
`every_persistent_table_has_a_projection_scope` requires classification of each persistent
table, and `shared_folds_never_order_by_local_arrival` rejects literal shared arrival folds by
default with explicit documented local/immutable-position exceptions. The source guard is a
first barrier; the fix must also centralize helper and in-memory key use so indirect pagination
or nonliteral SQL cannot bypass the invariant. Run all three with `cargo test -p st3 --lib canonical_audit -- --nocapture`. The final baseline run compiled successfully: table classification passed; the source guard named 62 shared arrival-order clauses; shuffle/restart/checkpoint comparison failed with the divergences above (1 passed, 2 intentionally failed).

The required sync invariants and these enforcing test names are stated in
[replication](replication.md#sync-invariants), linked from the README and fleet join guide.

The fix step must extend this foundation with non-empty fixtures for currently empty proposal/dependency tables, multiple writers/equal-time conflict ties, every nonmaterialized shared source, derived attention at fixed as_of, and production per-table incremental digests. It must check incremental insert/update/delete results against a full digest oracle, name differing tables in replication status/doctor, and ensure an unchanged table never needs a whole-table rescan. Restart/checkpoint tests must compare logical rows and each production digest, not just the legacy six-table graph_digest.

## Required fix boundaries

1. Shared canonical helper + exhaustive local exceptions + build guard against new unclassified/shared arrival folds. Retain deterministic ancestry/operation selection and local cursor semantics.
2. One incremental digest per logical shared table/source covering all shared columns, with insert/update/delete maintenance in the same transaction, stable serialization/PK order, full-rebuild validation and migration. Cheap cache invalidation alone is not incremental hashing. Physical indexes, receipt metadata, local clocks and live overlays stay outside. Extend replication status, heal diagnostics and doctor with named mismatches, and keep format/version compatibility explicit.
3. Expand shuffle/restart/checkpoint CI coverage to every source and derived attention view. The audit commit intentionally contains the failing regression; it is not a green merge candidate. Only the completed fix PR with st/ci green on its own head may join the smalltalk merge train.
