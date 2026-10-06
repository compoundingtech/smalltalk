# Conversation normalization design (#1561)

The project owner decided on 2026-10-06: no outbox/queue and no stored agent history; status
is a best-effort latest-wins value with a very short timeout. Conversations are
read from their owner machine at request time, including managed seats. cos owns
a separate mission to remove the outbox and stored transcript operations. The
first implementation includes this design; its wire contract still goes
to the project owner via the curator for exact-head approval before implementation is queued.
If a harness shows content in its terminal, st shows it in the conversation. Normalize
for stui, web and phone without token filtering, secret scrubbing, allow-lists or
fail-closed sanitizers. Access checks remain. Show only reasoning the harness exposes;
never invent or obtain provider-hidden reasoning.

## One display contract

Keep the timeline envelope (`id`, source `sequence`, `revision`, `timestamp`, `role`,
`type`, `final`, `body`) and use a negotiated `body.blocks` array. Each block has
`id`, `kind`, `source_type` and `payload`; optional `continuation` describes an
owner-fetched remainder. Kinds are text, reasoning, tool_call, tool_output, image,
job, subagent, ask, status, image_link and unknown. For standard known bodies,
`payload: {body_ref: true}` refers to the containing entry's fallback body, avoiding
an identical second copy; a continuation still fetches the original full body. Preserve native identities, call/result
correlation, revisions, supplied timing and failure state. Job/subagent payloads
preserve supplied parent/activity links; an historical ask is never a live picker.
Unknown uses `payload: {raw: <original JSON>}` and retains the native type and role
when supplied. Future fields and full structured tool arguments survive; unknown
blocks have a readable JSON view. Rust and phone adapters preserve tagged text and
raw system/unknown fallbacks; structural mail/command envelopes can still normalize
into their existing display shapes. Setup-like XML context is not stripped from
entries the API has already exposed. Unknown role means unknown attribution, not omission.

### Native record visibility audit

Omit only established native setup metadata or an explicit harness-hidden marker.
When visibility is unproven, retain the raw record instead of guessing it is internal.

| Harness records | Decision and reason |
| --- | --- |
| Codex `session_meta` | Omit the session identity/cwd setup header; it is not a terminal turn. |
| Codex `event_msg`, `turn_context`, `token_usage_record`, `world_state`, response `ghost_snapshot` | Retain raw; terminal visibility varies by native subtype/release. No generic exclusion of event mirrors. |
| Codex message roles `system`/`developer` | Omit provider bootstrap instructions, not terminal chat turns; unknown/future roles are retained raw. |
| Claude `progress`, `file-history-snapshot`, `file-history-delta`, `queue-operation`, `permission-mode`, `mode`, `atis-latch`, `last-prompt`, `ai-title`, `custom-title`, `cost-state`, `agent-name`, `tag`, `pr-link`, `bridge-session`, `fork-context-ref` | Retain raw; no demonstrated generic harness-hidden rule. |
| Claude attachments other than queued commands and system records without text | Retain raw; queued prompts and textual notices keep their known display shape. |
| Pi/OMP `session` | Omit the native session setup header. |
| Pi/OMP `custom_message` with explicit `display: false` | Omit because the native extension explicitly marks it hidden from its terminal. |
| Pi/OMP `custom`, `label`, `session_info`, `credential_pin`, `title`, model/thinking changes and summary records without text | Retain raw; summaries with text remain readable native notes. |
| OpenCode `snapshot` | Retain raw; visibility is not established across releases. |
| Any unknown record/block/role | Retain original JSON and original attribution with a visible unknown label. |

These decisions avoid silent per-type exclusions. Bounded native input windows and
undecodable records still produce visible size/read notices; they are transport
limitations. Oversized OpenCode items receive bounded display stubs and full owner
continuations, rather than being dropped before ref generation.

Start in `crates/st3/src/external_sessions.rs`; use the same normalizer for owner
side-input updates, reads and follow. Remove deliberate visible-reasoning exclusions, image
withholding and the 512-byte unknown excerpt policy. No sanitized-text payload class
or second durable transcript is needed. Native transcript formats remain authoritative.

## Owner reads, images and limits

Bind an exact seat/session/incarnation and native file identity on its owner machine.
HTTP pages, follow, search and image/body fetches use that binding and conversation
access control through existing owner forwarding. No newest-file guesses. The
conversation reports **owner unavailable** when unreachable; a reachable owner with
a missing transcript reports **transcript unavailable**, rather than an empty session.
Neither state substitutes stored transcript operations or an old prepared page.

Use `GET /v1/client/conversations/{session}/content/{ref}/chunk?offset=N` for
image bytes and oversized block bodies. The opaque ref is scoped to the authorized
session, entry/revision and owner source identity, not an arbitrary path or URL.
Return bounded bytes with media type, total size and next offset; check authorization
and binding on every fetch. Inline/base64 pixels and files in the bound Pi/OMP
content-addressed provider blob store can be read on demand. Blob digests are
verified on every fetch, so changed pixels cannot be joined across chunks. A transcript `file://` URI grants no
access outside that store; symlink escapes and non-regular files are rejected.
The daemon never requests transcript HTTP(S) URLs, including their redirects;
`image_link` retains the original URL and JSON for explicit opening by the client.
Detected PNG/JPEG/GIF/WebP signatures determine image MIME; native MIME labels are
ignored. SVG/HTML and unrecognized bytes remain fetchable as opaque octets and
must never be rendered as active image/HTML content. Reuse transport/chunk
machinery, not message-attachment authority: a native image need not have a graph
message ID. Native absence or fetch failure is availability, not content scrubbing.

`read.projections` is the raw-transcript read scope. It authorizes full native
reasoning, arguments/output, unknown JSON and image bytes, including secrets the
agent saw. This includes anonymous local read-only Unix sessions, identified local
actors, and projection-only paired display/phone keys. There is no separate secret
filter or raw-content scope. Pairing scope changes and revocation apply to every
chunk request, including owner-forwarded requests.

The present 8 KiB/value and 1 MB/page bounds protect memory and transport only.
The initial chunk contract returns up to 256 KiB decoded bytes; image reads have
a visible 32 MiB decoded limit. Four expensive owner timeline/chunk reads can run
concurrently; excess reads, including managed timeline reads, return HTTP 429
`rate-limited`, without a queued backlog. Clients must back off and retry.
An authenticated ref contains a JSONL offset/length/digest or SQLite part/message
identity/digest (message role/time, independent of streaming usage updates),
entry/revision, session and stable native file identity. Its encrypted source
descriptor carries the owner-located driver, native ID and transcript path; the
client cannot see or choose that path. No conversation bytes are carried in the
ref. Native chunks use this descriptor directly, without inventorying other
sessions. Managed chunks retain current owner binding checks. Each chunk reads and
normalizes its identified record once, then rechecks source identity after fetching
bytes. It returns the validated record snapshot; content-addressed blob hashes bind
external pixels to that snapshot. A concurrent edit after capture is detected on
the next fetch. No whole timeline rebuild or second record decode occurs.
Appends, including SQLite WAL growth, preserve existing refs;
record edits, file replacement, managed binding changes or an owner process restart return
`conversation-content-invalidated` with `full_resync: true`. The ephemeral owner
authenticated-encryption key is neither stored nor replicated. Clients must reload
the timeline after a daemon restart to obtain new refs. Timeline reads capture a finite native
high-water mark; concurrent appends belong to the following read.
Count encoded response bytes, preserve valid JSON/UTF-8, and mark every clipped
value with its reason, original size when known and continuation. Fetching the
remainder recovers full arguments/output/unknown JSON; do not cut JSON into an
apparently complete object. Oversized entries must make progress through chunks.
File/line scan bounds and missing native history also need explicit partial notices.
Paging cursors fence source revisions; invalidation requests resync, never silently
skips content. Build on #1487's versioned per-subject incremental read models, with
native transcripts as explicit owner-local side inputs. Maintain append/revision
folds as the source changes, and read the normalized model at request time rather
than re-normalizing the whole file for every page. Target wakes to affected sessions;
merged #1512 provides mailbox dependency wakes, not a completed conversation IVM.

The conversation model, prepared pages and search content remain bounded volatile
memory: #1487's proposed SQLite-backed shape cannot persist conversation bytes under
the project owner's decision. Rebuild after restart/eviction from the native source, without a
durable ingestion queue or second transcript. Validate owner availability and file
identity/revision on requests; bring the fold to the captured native high-water mark
or report explicit partial progress. Replacement/truncation invalidates its old basis.
A model is a derived accelerator, never authority or an offline fallback. Coordinate
this boundary with #1487's owner, avoiding a second cache implementation; prove bounded
cold rebuild, incremental append/replace work and unchanged-read costs separately.

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

The decided target is **no stored agent history and no outbox/queue**, including
local observation databases, replicated transcript claims and durable event spools.
The harness's own native transcript on its owner remains the conversation source.
Categorical status is one small replace-in-place value per seat, fenced to the active
runtime and ordered by original source time. Publication is best effort with a very
short timeout, no retry backlog: a failed send is superseded by the next current
observation. Stale evidence becomes unknown; the removal mission specifies the timeout
and freshness bound. The no-agent-history builder confirmed 100 ms one-shot local/fleet attempts and
source-age freshness on 2026-10-06. Numeric usage/limits and st messages remain
durable independently. No prose, arguments, output or ask prompt belongs in that value.
A latest-shaped append claim alone does not meet the replace-in-place requirement.
Coordinate #1546 with its author and cos; do not build a competing lane or assume its
earlier FIFO/history guarantee survives this decision.

## Interface with the removal mission

`st missions ls` was checked before editing this design; cos's new removal mission
was still being drafted then. It is now published as
`mission-run/fleet/smalltalk/no-agent-history/2026-10-06`; its builder confirmed
that it will sequence reader cutover against this normalizer/chunk contract.
Check the mission list and its published ownership before
any implementation touching managed storage, admission, status or the outbox.

Normalization needs these outcomes from cos's mission:

1. An exact owner/session/incarnation/native-source binding for managed reads, with
   authorized forwarding and a versioned native side input usable by #1487's
   incremental model, paging, follow and chunk fetches. No dependence on stored
   timeline operations or an outbox sequence.
2. Explicit owner-unavailable and transcript-unavailable results; stale prepared pages
   cannot masquerade as current owner reads. Source replacement invalidates refs and
   cursors visibly. Search uses the same owner availability and volatile-only content.
3. A current-status read containing categorical value, source time, incarnation and
   freshness/unknown state. Conversations read it without historical status replay.
   Historical harness-shown status comes from the native source when present.
4. A cutover signal for flushing old pages/search stamps/replay cursors. Numeric usage
   accounting/OTLP consumers must be handled independently, without retaining agent
   conversation history or exporting its bodies to preserve the old carrier.

cos owns the removal order: establish owner-read/current-status replacements and
dependent-reader compatibility, stop all old/new producer and admission paths,
then retire operation reducers/indexes and clean existing data. Its mission must
account for pending prepared events and numeric usage, legacy polled providers,
status-history API retirement, mixed builds, rollback and old writers that would
reintroduce storage. Do not silently report discarded pending events as delivered.
It also owns inventory/cleanup of local rows, spools, legacy files, replicated
envelopes and backups. Checkpoint guards, whole signed envelopes, pinned/evidence
references and shared operations can block deletion; report residual data. A rules
change needs coordinated version/digest agreement (baseline 11; #1473 reserves 12).
The normalization mission performs none of this removal or live-store cleanup.

## Client compatibility

Existing stui/iOS clients must stay usable. The frozen old-Swift decode fixture is
hand-authored from six contract cases, not generated by the normalizer. Separate
real owner HTTP tests exercise normalizer-produced legacy/negotiated responses,
full chunk reads, paired credentials and authenticated fleet forwarding. Swift currently decodes a closed
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
the owners' work: #1487 (incremental per-subject read models; coordinate its
conversation-content persistence boundary), merged #1512 (targeted mailbox wakes),
#1323/#1351 (native semantics and chronological windows/cursors),
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
superseded and must not enter this stack. Coordinate #1546 separately: the project owner's
no-outbox/latest-status decision supersedes the earlier history lane plan; cos and
the collaborating assistant own reconciliation and removal, not this normalization builder.

Normalization PRs: contract/normalizer and withholding removal; owner chunk/image
fetch; shared/generated clients and renderers. Managed reader/writer cutover and
existing-data cleanup belong to cos's separate mission. Each contract, claim or
checkpoint change returns for exact-head approval through its owning mission.
Use isolated daemons and invented fixtures to compare harness-shown content through
HTTP, follow/reconnect and all renderers, including full arguments, exposed reasoning,
unknown nested JSON, images, oversized chunks, paging, owner failure and mixed builds.
Prove no new content writes, preserved accounting/retries, and checkpoint convergence
before cleanup. Required checks run on each implementation head; no self-enqueue
before contract approval, no manual merge or deployment from this document.

## First implementation boundary

The first PR establishes negotiation, raw native blocks, legacy fallbacks and
on-demand owner chunk reads with revision invalidation. It removes the #1453/#1482
withholding policy without changing producers, admission, current-status storage
or the managed stored fallback. It exposes the normalizer/preparation interface
for #1487's native side-input fold. This checkout contains targeted reconcile
wakes but no conversation IVM engine; the first slice still uses the existing
bounded native read path. It claims no cold-read or incremental-append performance
proof. Integrate with #1487's owner before the subsequent read-model slice; do not
add an independent cache or persist conversation bytes to meet that dependency.
