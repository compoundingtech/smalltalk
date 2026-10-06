# Conversation normalization proposal (#1561)

Proposed for Nathan's exact-head approval; this document changes no runtime behavior.
If a harness shows content in its terminal, st shows it in the conversation. Normalize
for stui, web and phone without token filtering, secret scrubbing, allow-lists or
fail-closed sanitizers. Access checks remain. Show only reasoning the harness exposes;
never invent or obtain provider-hidden reasoning.

## One display contract

Keep the timeline envelope (`id`, source `sequence`, `revision`, `timestamp`, `role`,
`type`, `final`, `body`) and propose a negotiated `body.blocks` array. Each block has
`id`, `kind`, `source_type` and `payload`; optional `continuation` describes an
owner-fetched remainder. Kinds are text, reasoning, tool_call, tool_output, image,
job, subagent, ask, status and unknown. Preserve native identities, call/result
correlation, revisions, supplied timing and failure state. Job/subagent payloads
preserve supplied parent/activity links; an historical ask is never a live picker.
Unknown uses `payload: {raw: <original JSON>}` and retains the native type and role
when supplied. Future fields and full structured tool arguments survive; unknown
blocks have a readable JSON view. Unknown role means unknown attribution, not omission.

Start in `crates/st3/src/external_sessions.rs`; use the same normalizer for owner
reads and follow updates. Remove deliberate visible-reasoning exclusions, image
withholding and the 512-byte unknown excerpt policy. No sanitized-text payload class
or second durable transcript is needed. Native transcript formats remain authoritative.

## Owner reads, images and limits

Bind an exact seat/session/incarnation and native file identity on its owner machine.
HTTP pages, follow, search and image/body fetches use that binding and conversation
access control through existing owner forwarding. No newest-file guesses. The
conversation reports **owner unavailable** when unreachable; a reachable owner with
a missing transcript reports **transcript unavailable**, rather than an empty session.
Neither state substitutes stored transcript operations or an old prepared page.

Propose `GET /v1/client/conversations/{session}/content/{ref}/chunk?offset=N` for
image bytes and oversized block bodies. The opaque ref is scoped to the authorized
session, entry/revision and owner source identity, not an arbitrary path or URL.
Return bounded bytes with media type, total size and next offset; check authorization
and binding on every fetch. Resolve native blobs, inline/base64 bytes and harness-shown
image links on the owner. Reuse transport/chunk machinery, not message-attachment
authority: a native image need not have a graph message ID. Source changes invalidate
refs visibly. Native absence or fetch failure is availability, not content withholding.

The present 8 KiB/value and 1 MB/page bounds protect memory and transport only.
Count encoded response bytes, preserve valid JSON/UTF-8, and mark every clipped
value with its reason, original size when known and continuation. Fetching the
remainder recovers full arguments/output/unknown JSON; do not cut JSON into an
apparently complete object. Oversized entries must make progress through chunks.
File/line scan bounds and missing native history also need explicit partial notices.
Paging cursors fence source revisions; invalidation requests resync, never silently
skips content. Prepared pages and search indexes may be bounded volatile memory;
validate owner/source availability at request time, and persist no conversation bytes.

## Managed seats currently store content

“Read from the owner, as today” describes native sessions, not all managed seats.
`client_v0.rs` first tries a bound native transcript, then reconstructs a claim-backed
fallback, showing only the newest 4,096 `harness.timeline` operations. Append/replace/
finalize chains, truncation coverage and `timeline-query-limited` support that fallback.

Today's dependencies that must move before removal:

- `st-drivers` commits timeline state and events to `st-harness-events.sqlite`;
  `main.rs` publishes the FIFO with prepared retry bodies and successful-prefix
  acknowledgment. Adopted older providers also use the polled timeline record file.
- Timeline pages/deltas, WebSocket replay cursors and conversation-search stamps
  consume stored operations. Replace these with owner source revisions and visible
  availability; preserve chronology and graph-message deduplication.
- Response usage deduplication, totals/spend, `harness.usage` rollups and OTLP consume
  usage-shaped timeline operations. Move numeric accounting to a separate metadata
  path with the same attribution and retry identity before retiring that carrier.
  Do not export conversation bodies through the observation exporter.
- Local observations default to seven days/20,000 per subject/kind, retaining the
  newest. Older builds can still replicate the kind. Checkpoint rules currently keep
  the newest legacy timeline claim and age other candidates at five days before the
  cut, subject to witnesses and whole-envelope/evidence guards. UI limits do not
  delete rows. Status-transition readers also depend on historical observations.

The target is **no managed conversation content in local observation databases,
replicated claims, durable event spools or payload stores**. Keep categorical current
status as one small replace-in-place value per seat, fenced to the active runtime and
ordered by original source time; repeated observations replace that value. No prose,
arguments, output or ask prompt belongs in it. Reuse #1546's `harness.current` work
rather than introduce a competing lane. A Latest claim alone is still append-only:
bounded physical storage needs its reviewed replacement witness/checkpoint protocol,
not merely a latest-shaped API. Source-time winners must survive delayed replay.

## Removal order and compatibility

1. Post this design and open the contract PR for Nathan via the curator at its exact
   head. No storage, reader, outbox or retention removal before approval. Coordinate
   the contract and outbox boundary with Johannes's assistant through #1546.
2. Add normalization, chunk fetch and renderer support with capability negotiation;
   prove owner-native reads for managed seats and move accounting/OTLP dependencies.
   Keep #1546's approved independent current lane and interim FIFO/gap guarantees
   intact while this replacement is prepared; do not duplicate its cap repair.
3. Roll out readers and owner routing, then switch managed conversations/search/follow
   to owner sources. Flush held legacy pages/cursors with visible resync. Prove owner
   outages, restart/rebind and old-client fallback before removing content publication.
4. Upgrade producers/drivers and admission together to stop queuing/publishing content,
   including the legacy polled path. Account for every pending event: preserve prepared
   retry identities, acknowledgment order and numeric usage. An approved migration may
   retire content events with an explicit cutover record; never silently clear the
   shared spool or label discarded events delivered. Old writers must be fenced from
   reintroducing content. Coordinate provider restarts/rollback; old drivers cannot
   adopt event-producing providers merely because a daemon upgraded.
5. Only after consumer migration, retire conversation operation reducers/indexes and
   their admission/schema paths. Latest-only status also needs an explicit decision
   about existing status-history/coverage guarantees: retain required control history
   during transition, then deprecate dependent APIs before deleting it. This design
   does not silently revoke #1546's ordered-history contract.
6. Inventory existing local rows, spools, legacy files, replicated envelopes and backups.
   Clean content through a separately reviewed migration/checkpoint rule; preserve
   accounting, durable Small Talk messages and unrelated mission evidence. Signed
   envelopes delete whole, and pinned/evidence/shared claims may block cleanup: report
   remaining bytes. Backups retain historical content until their agreed disposal;
   do not claim past copies are erased. Coordinate the rules bump (baseline 11; #1473
   reserves 12), mixed-member checkpoint agreement and late old-writer replay. No
   automatic live-store purge, fleet restart or backup deletion follows this proposal.

Existing stui/iOS clients must stay usable. Swift currently decodes a closed
`TimelineType` enum, so sending new top-level kinds to old clients is breaking.
Advertise/negotiate blocks and chunk fetch; retain existing entry types and complete
text/JSON fallbacks with explicit transport truncation notices for legacy clients.
New clients render reasoning, full expandable arguments/results, unknown JSON,
images, jobs/subagents, asks and availability consistently. Old clients keep their
known text/tool/status behavior; they do not gain image fetch or new interactive
rendering. Only a genuinely incompatible wire change requires rejecting an old
client. Old daemons still have stored fallback behavior until upgraded; the no-storage
guarantee begins after writer cutover, not on a mixed fleet's first deploy.

## Existing work and proof

Open conversation work was inventoried on 2026-10-06. Sequence after or integrate
the owners' work: #1323/#1351 (native semantics and chronological windows/cursors),
#1349/#1348 (exact binding/revisits), #1446 (cold runtime/message lookup, currently
parked), #1471 (volatile prepared owner pages, stacked on #1446), #1458/#1478
(timing/outcomes), #1496/#1442/#1342 (live ask identity/actions), #1444/#1441
(tasks/queue), #1343 (generated transport), #1205 (search coverage), #1521
(owner terminal history), #1489 (terminal images/input), and #1173 (mail images).
These are dependencies/coordination points, not approval or reasons to duplicate them.

#1453's omission labels remain useful, but dropping unknown-role text and hiding raw
unknown bodies contradict #1561. Merged #1482 deliberately withholds image bytes/URLs;
replace that policy with authorized owner fetch. Closed #1507/#1560's unknown-block
allow-list and argument withholding, and #1449's sanitizer/capture slice 1, are
superseded and must not enter this stack. Coordinate #1546 separately: it remains
the authorized interim status repair, not permission to remove historical content.

Small PRs: contract/normalizer and withholding removal; owner chunk/image fetch;
shared/generated clients and renderers; managed reader/writer cutover; existing-data
cleanup. Each contract, claim or checkpoint change returns for exact-head approval.
Use isolated daemons and invented fixtures to compare harness-shown content through
HTTP, follow/reconnect and all renderers, including full arguments, exposed reasoning,
unknown nested JSON, images, oversized chunks, paging, owner failure and mixed builds.
Prove no new content writes, preserved accounting/retries, and checkpoint convergence
before cleanup. Required checks run on each implementation head; no self-enqueue
before contract approval, no manual merge or deployment from this document.
