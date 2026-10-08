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
job, subagent, ask, status, error, document, source_record, raw_text, image_link and unknown. For standard known bodies,
`payload: {body_ref: true}` refers to the containing entry's fallback body, avoiding
an identical second copy; a continuation still fetches the original full body. Preserve native identities, call/result
correlation, revisions, supplied timing and failure state. Job/subagent payloads
preserve supplied parent/activity links; an historical ask is never a live picker.
Errors also have a known `error` block with `{body_ref:true}`, so native stop/exit
notices from #1478 need no new kind. Optional `block.metadata` is an open JSON
object for source-supplied timing (including `wallTimeMs` and `timeoutSeconds`),
preserving original names, units, values and future fields. Native tool-result details
and assistant text/reasoning metadata use that same shape.
Unknown uses `payload: {raw: <original JSON>}` and retains the native type and role
when supplied. Future fields and full structured tool arguments survive; unknown
blocks have a readable JSON view. A `source_record` block with the `internal`
visibility hint retains the entire parsed native record, including fields unused by
the typed projection. A `document` block carries the original Claude document,
rather than only a placeholder. The optional open-string `visibility` field is a
UI hint (`visible` by default, `internal`, or `hidden-by-harness`), never a read-scope
restriction. Unknown role means unknown attribution, not omission.

Malformed JSON, unfinished tail lines, blank lines and unknown encodings become
`raw_text` blocks. Their payload has `encoding: "base64"`, `bytes` containing the
exact original bytes (including newline and controls), and a lossy `text` preview.
OpenCode message and part data are read as SQLite bytes, including BLOB/invalid
UTF-8 values, and preserved by the same rule. Replacing/completing a partial record
invalidates its old ref visibly; appending another record preserves previous refs.
A torn prefix can produce both a raw block and a recovered typed record.

### Native record visibility audit

The sole omission table is `omission_reason` in
`crates/st3/src/external_sessions.rs`: one named match arm per permitted type with
an explicit reason. It omits only the four categories below. Nothing else may be
dropped by a parser branch: an otherwise empty projection falls back to raw JSON.
The coverage test uses harness-specific native record fixtures, the redacted OMP
resume/tool captures (also exercising Pi’s shared format), and message/part objects
from the OpenCode 1.18.34 admission capture. It fails if an empty result has no
table justification; separate probes cover unknown types, malformed bytes and
direct owner reads. Fixture provenance is in `crates/st3/fixtures/native-records/SOURCES.md`. When visibility is unproven, retain it.

| Harness records | Decision and reason |
| --- | --- |
| Codex `session_meta` | Omit the session identity/cwd setup header; it is not a terminal turn. |
| Codex `event_msg`, `turn_context`, `token_usage_record`, `world_state`, response `ghost_snapshot` | Retain raw; terminal visibility varies by native subtype/release. No generic exclusion of event mirrors. |
| Codex message roles `system`/`developer` | Omit provider bootstrap instructions, not terminal chat turns; unknown/future roles are retained raw. |
| Claude `progress`, `file-history-snapshot`, `file-history-delta`, `queue-operation`, `permission-mode`, `mode`, `atis-latch`, `last-prompt`, `ai-title`, `custom-title`, `cost-state`, `agent-name`, `tag`, `pr-link`, `bridge-session`, `fork-context-ref` | Retain raw; no demonstrated generic harness-hidden rule. |
| Claude attachments other than queued commands and system records without text | Retain raw; queued prompts and textual notices keep their known display shape. |
| Pi/OMP `session` | Omit the native session setup header. |
| OMP `reset_boundary`, `credential_pin`, `title` | Retain raw plus typed status views; reset/title are quiet events, credential pins are internal bookkeeping hidden by default. |
| Pi/OMP `custom_message` with explicit `display: false` | Omit because the native extension explicitly marks it hidden from its terminal. |
| Pi/OMP `custom`, `label`, `session_info`, model/thinking changes and summary records without text | Retain raw; summaries with text remain readable native notes. |
| OpenCode `snapshot` | Retain raw; visibility is not established across releases. |
| Claude documents | Retain the complete document payload, with a readable document fallback. |
| Malformed/partial/unknown-encoding input | Retain exact bytes as a `raw_text` block; never silently skip a tail line. |
| Any unknown record/block/role | Retain original JSON and original attribution with a visible unknown label. |

These decisions avoid silent per-type exclusions. Bounded native input windows and
read failures still produce visible size/read notices; they are transport
limitations. Unparseable records within the input bounds remain accessible as raw bytes.
OpenCode items whose native rows fit the input bound receive bounded display stubs
and full owner continuations. Rows exceeding that bound produce explicit
not-fetchable notices, rather than disappearing or acquiring unusable refs.

Start in `crates/st3/src/external_sessions.rs`; use the same normalizer for owner
side-input updates, reads and follow. Remove deliberate visible-reasoning exclusions, image
withholding from the data. A 512-character unknown excerpt is a UI preference only. No sanitized-text payload class
or second durable transcript is needed. Native transcript formats remain authoritative.

## Typed views and conversation header

Everything here is additive and optional, on top of the #1574 block contract, so an
old client ignores the new fields and keeps the existing text fallback. A block MAY
carry `view: {type: <string>, ...}`; `type` is an open discriminator, and the client-v0
schema encodes one closed definition per known `view.type` (`TimelineView*` in
`docs/st3/client-v0/schemas/client-v0.schema.json`) plus a `TimelineView` fallback
branch that accepts any other type string. A renderer that does not know a type
renders the block as it does a block without a view. `view` holds parsed fields only;
the full native arguments or output stay in `payload`, exactly as in #1574. Raw JSON
stays available for every record, `view` is computed per record and deterministic,
and the fold caches it with the entry. Generated Rust, Swift and TypeScript clients
carry `view` and the header below as loose JSON values (`Option<Value>`, `JSONValue?`,
`unknown`), so no client decoder gains closed cases.

`conversation_blocks::prepare_one` bounds both `view` and `metadata` after enriching
child-session links, for full reads and native keyset pages alike. Their outer object
and keys (including `view.type`) survive; long strings carry the visible size-limit
marker, and oversized nested objects/arrays may become marked JSON-preview strings.
Generated clients retain these as loose JSON, and both conversation renderers check
string/array/object shapes rather than assuming a closed view. A single clipped open
subtree uses a continuation to `/body/blocks/{index}/view` or `/metadata`; if another
remainder already needs that block's continuation, it instead points to the whole
original `/body` so owner fetch recovers every exact subtree. Clients inspect the
returned JSON accordingly. The final entry-size guard still runs after all display
bounding and can replace an over-budget entry with a fetchable error notice.

On `tool_call` blocks, parsed from the native arguments (OMP field `i` becomes
`intent`; every tool_call view also has `tool`, the native tool name):

| view.type | fields | OMP source |
| --- | --- | --- |
| `bash` | command, cwd?, timeout_s?, env_keys?: string[], background: bool | tool `bash` {command,cwd,timeout,env,async} |
| `edit` | path?, ops: number, input_bytes | tool `edit` {input} (hashline patch; path = first `[PATH#TAG]` header) |
| `write` | path, bytes, content?, line_count? | tool `write` {path,content}; line count counts native written lines |
| `read` | path, range?: string | tool `read` {path} (suffix after `:` = range) |
| `search` | engine: "grep"\|"glob"\|"web", pattern?, path?, query?, case?, hidden?, gitignore?, limit?, skip? | tools `grep`, `glob`, `web_search`; flags preserve explicit false/zero values |
| `todo` | op, items?: [{content, status}], phase?, task? | tool `todo` |
| `ask` | questions: [{id, question, options: [{label, description?}], multi: bool, recommended?: number}] | tool `ask`; every alternative survives the selected answer |
| `task` | context?, tasks: [{name?, agent?, task}] | tool `task`; context carries the parent context/contract |
| `hub` | op, name?, target?, timeout_s?, message? | tool `hub`; send body remains beside its receipt |
| `eval` | language, title?, code_bytes, code?, timeout_s?, reset? | tool `eval`; preserve explicit reset=false and timeout=0 |
| `generic` | name | any other tool |

On `tool_output` blocks, parsed from the OMP toolResult `details`; all of these also
have `tool`, `call_id` and `is_error`:

| view.type | fields |
| --- | --- |
| `bash` | exit_code?, wall_ms?, timeout_s?, timed_out?: bool |
| `search` | engine, match_count?, file_count?, truncated?, file_limit_reached?, per_file_limit_reached?, warning?; counts come from native matchCount/fileCount, never inferred from bounded rows |
| `edit` | path?, first_changed_line?, diff?: string (unified diff as given) |
| `todo` | phases: [{name, items: [{content, status}]}] |
| `ask` | answers: [{question, selected: string[], custom?: string, note?: string}] |
| `task` | async: bool, total_ms?, agents: [SubagentSummary] |
| `hub` | op, timed_out?: bool, jobs?: [JobSummary], state? |
| `generic` | is_error: bool, wall_ms? |

`SubagentSummary` = `{id, name?, agent?, status, task?, duration_ms?, tokens?, cost_usd?,
requests?, tool_count?, conversation?: {session_id}}`. `name` preserves the OMP name
(native `name`, otherwise `id`); `agent` is its type. `task` preserves the full native
`assignment`, otherwise `task`. Both clients retain invocation rows when the result
arrives, draw all assignment lines on expansion, and label the child-open row with
the name while routing to the actual session id. The owner fills
`conversation.session_id` only when the child transcript exists at
`<parent transcript without .jsonl>/<id>.jsonl` and is readable; that session id opens
through the normal conversation routes (same fold, paging and refs), which is how a
subagent card links to the child transcript as its own conversation.
`JobSummary` = `{id, name?, type?, state, exit_code?, started_at?, ended_at?,
duration_ms?, output_bytes?}`.

Extension and bookkeeping records become blocks as follows; anything not listed keeps
its #1574 shape (other `custom_message` records with display != false stay `text`):

| OMP record | block kind | view |
| --- | --- | --- |
| custom_message `irc:incoming` | `irc` (new kind) | `{type:"irc", from, message, reply_to?, message_id}` |
| custom_message `launch-completion` | `job` | `{type:"job", jobs: [JobSummary]}` (from details.daemons) |
| custom_message `async-result` | `job` | `{type:"job", jobs: [JobSummary]}` (from details.jobs) |
| custom_message `skill-prompt` | `status` | `{type:"skill", name, path?, args?}` |
| `compaction` / `branch_summary` | `status` | `{type:"compaction", method?, tokens_before?, tokens_after?, short_summary?, summary?}`; first lines collapsed, full summary on expansion |
| `model_change` | `status` | `{type:"model_change", model, role?, fallback?: bool}` |
| `thinking_level_change` | `status` | `{type:"thinking_level", level, configured?}` |
| `title_change` / `title` | `status` | `{type:"title", title, previous?, source?}` (`previous` only on title changes) |
| `reset_boundary` | `status` | `{type:"reset_boundary"}`; quiet event `session reset` |
| `credential_pin` | `status`, visibility `internal` | `{type:"credential_pin", provider}`; hidden by default; hash remains only in raw payload/source record |
| custom `session_exit` | `status` | `{type:"session_exit", kind, reason}` |
| custom `tool_execution_start` | `status`, visibility `internal` | `{type:"tool_start", call_id, tool, started_at}`; renderers attach it to the call row and do not draw it alone |
| assistant message metadata | on its `text`/`reasoning` blocks | `metadata.model`, `metadata.provider`, `metadata.usage` {input, output, cache_read, cache_write, total, cost_usd}, `metadata.context_tokens`, `metadata.stop_reason`, `metadata.ttft_ms`, `metadata.duration_ms`; no view |
| assistant error message | `error` | `{type:"assistant_error", status, presentation, is_error, label, message, stop_reason?, error_id?, api?, provider?, model?, retry?}` |

Assistant errors come from the assistant session message, not a synthesized daemon
failure. Persisted `retry` markers carry `{kind,status,attempt,recovery,note,
recovered_at?,superseded_by?}`. Status is `failed`, `recovered`, or `superseded`;
presentation is `full`, `compact-recovered`, or `none`, matching OMP's persisted
outcome. Original error text remains available on expansion after recovery.
Superseded errors are retained in source data but not drawn. No in-progress retry
state is invented when the session record does not contain it.

Written content, assignment/context, eval code and compaction summaries use the
existing 8 KiB display bound and authenticated continuation. Typed view-only refs
are reattached to their original blocks and passed through the same adapter on
expansion; metadata and unknown subtrees keep a raw JSON fallback. The written
line count describes the complete source, not the bounded preview.


Timeline pages and delta responses (when `conversation-blocks.v1` is negotiated) MAY
carry a `header` object. Each field is `{value, source: register|transcript, as_of}`:
`model` (model id), `context` ({tokens, window}), `cost` ({usd}, with `window: true`
when the fold window is truncated), `todos`, `jobs` ([JobSummary]), `subagents`
([SubagentSummary]), `ask` (the last ask call without a matching result, or null) and
`working` (bool). `source` is `register` for the live latest-wins value from #1583 or
`transcript` for a value derived from the fold window; a field is absent when neither
source has it, `working` comes only from the register, and the register wins over the
transcript once it is live. The register adapter is one function that returns `None`
until #1583 lands. Transcript derivation: model = last assistant `model`; context =
last `contextSnapshot.promptTokens`; cost = sum of `usage.cost.total` in the window;
todos = last todo tool_output view, else the last todo call; jobs = latest state per
job id from job blocks and hub outputs, keeping non-terminal ones; subagents = task
outputs whose status is not terminal; ask = the last ask call without a matching
result. `docs/st3/client-v0/fixtures/timeline-views.json` fixes the wire shape with a
synthetic page that carries views, an `irc` block and a full header.

The Rust and TypeScript renderers show every populated header field in the same fixed
order. The most common source appears once at the end (`· transcript · 2d ago`);
ties use the first source in field order. Only fields from another source get their own
marker (`cost $0.02 [register · 30m ago]`). The shared age uses the oldest field from
that source, so compaction cannot hide stale data. Phone headers wrap without a line
limit. A subagent's open-child link is an independent accessible control, outside the
card's expand/collapse control; opening it pushes a conversation so Back restores
the parent. Todo statuses remain distinct: `[x]` completed, `[ ]` pending, `[~]`
in progress, `[!]` blocked and `[/]` abandoned. Only `completed` counts as done.

## UI filters and show-everything mode

Rust `conversation_with_filters` and TypeScript `conversationEntries` accept an
explicit named filter set. `DEFAULT_FILTERS` selects harness-markup, context-block,
control-character, internal-block and excerpt filters. This keeps stui and the
phone's familiar delivery/task/command display shapes, folded tools/mail and
heartbeat/usage suppression, while new reasoning, image/link, document,
job/subagent/ask/status and unknown blocks have readable fallbacks. Default filters
hide XML reasoning/internal/system-reminder/tool wrappers, setup context wrappers
(permissions, environment, collaboration, skills, app/plugin and command metadata),
control characters and `internal`/`hidden-by-harness` block payloads; unknown text
has a visibly marked 512-character excerpt. Diagnostic and tool display bounds
remain UI choices. These operations act only on a presentation copy.

`SHOW_EVERYTHING` is the empty filter set: every entry, including message metadata,
usage/status, original JSON blocks and raw byte payloads, is rendered as reversible
JSON without folds, excerpting or markup removal. JSON escapes controls so a raw
terminal view can expose them without interpreting them. Decoding that JSON yields
the unchanged normalized entry. Transport size caps and owner continuations still
apply; raw mode cannot conjure an unrequested chunk.

This PR supplies the shared renderer APIs and verifies both defaults and raw mode.
Interactive raw-mode controls, and **stui/phone image loading and clipped-value
expansion wired to the chunk route, are the next implementation PR**. Current
clients show load/truncation markers and references; they do not fetch native image
pixels or expand clipped blocks yet. Existing graph-message image handling is
separate. Generated Rust/Swift/TypeScript clients expose the chunk API now.

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
Input bounds are distinct from response clipping. A JSONL read captures at most
32 MiB of source bytes and retains at most 4,096 complete lines from that window.
The partial first line and older prefix are marked **not fetchable through this
owner read**; there is no prefix continuation in this slice. A line exceeding the
window can therefore have no complete record in this response, with the same
explicit marker. Within the window, clipped values have fetchable continuations.

Open `block.metadata` retains its outer object and keys: strings over 8 KiB use
the existing `[st truncated this native timeline value: size limit; N bytes]`
suffix, and nested collections still over 16 KiB after leaf clipping become
clipped JSON-text previews. The block's existing `continuation` shape fetches
the original `/body/blocks/N/metadata` when no payload continuation is present;
an existing payload continuation takes precedence (a `/body` ref also includes
the original metadata). Error/status `message`, `details` and `detail` fallback
fields are bounded in the same preparation pass; strings remain strings and
negotiated body-ref blocks fetch the complete original `/body`. Legacy clients
receive the same visible clipping without blocks or continuation refs. A final
entry-size guard bounds remaining large body fields; an entry whose retained
keys still cannot fit becomes a visible `native-entry-too-large` error with the
same identity and ordering, without discarding its page's other entries.
Negotiated clients fetch that entry's exact original `/body`; legacy clients
receive the error message without its blocks.

A single 32 MiB text-heavy JSONL line is held several times during parsing,
source-record attachment, normalization and response preparation: budget roughly
150–250 MiB transient per read, or 600 MiB–1 GiB for four concurrent reads, in
addition to the daemon's other work. This is an estimate, not a strict RSS limit:
dense JSON, many projected parts, allocator overhead and encoded/base64 expansion
can exceed it. The source-byte limit and four-read admission limit are enforced;
a hard heap/AST budget and streaming normalization are follow-up work.

OpenCode streams selected messages and their parts instead of copying up to
4,097 message rows and every part into Rust vectors. Before any Rust byte copy,
the total ID/time/data cells of a message row, or ID/data cells of a part row,
must fit 32 MiB; `sqlite_bytes` also checks each cell. At most one accepted
message row and one part row are being normalized at a time, alongside the
32 MiB encoded/4,096-entry timeline suffix. Parsing and projection incur the
same transient-copy/AST overhead as JSONL. SQLite can materialize cells or sort
rows inside its engine before the borrowed-cell guard, so this is a Rust payload
copy bound, not a SQLite-engine or process RSS guarantee. Over-limit rows emit
`native-record-size-limit`, original source size and `fetchable:false`, visibly
**not fetchable**; a rejected message's parts also lack usable display context.
Direct chunk reads apply the same row bound; enlargement invalidates an old ref.
The native source is still authoritative and is not modified by these limits.

The OpenCode message-window query-plan regression uses the harness's
`message_session_time_created_id_idx(session_id, time_created, id)` from
[OpenCode v1.18.34](https://github.com/anomalyco/opencode/blob/aec0b9a6d8898f68f923aaf08b7306d931fd9d76/packages/core/src/session/sql.ts#L64-L76).
`EXPLAIN QUERY PLAN` shows an indexed session lookup in the inner query, with
only the already-limited outer window sorted chronologically (at most 4,097
rows). A read-only check of the installed native schema produces the same plan.
This guarantee assumes that composite index exists; older schemas without it
may sort session history before applying the limit. The regression exercises
the production SQL, including timestamp ties and session isolation.

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

#1478 (native stop/retry/abort/process exit) and #1458 (native tool-result timing)
will rebase onto this implementation's final head. The former currently proposes
withholding free-form stop/provider prose; that policy must be removed or kept
strictly as a UI presentation choice on rebase. Native source records here retain
all those fields. The latter's optional timing extraction can use open block
metadata; arbitrary native result details remain available in `source_record`,
without copying its numeric whitelist into this normalizer.

## Graph reads for conversation changes

Conversation routing uses a volatile map of runtime identities at eight recent
claim frontiers. A changed read folds only agents whose actual-state claims changed;
harness-only and unrelated commits do not repeat agent status/history reductions.
Catch-up copies at most 4,097 small subject/kind rows before releasing the statement;
a gap beyond 4,096 rebuilds the identity map. The map contains routing metadata,
not conversation bytes. A newly seen agent enters the map even when its first
claim is only a declaration or harness report, so warm and cold reads agree that
it exists without a current session. Projection repair/trim advances a generation
under the cache lock; a build started before that reset cannot republish its result.
Mixed origins and other actual kinds retain the existing canonical identity
reduction for that agent. This is not a history-independent
cold-read proof or a replacement for the shared incremental read-model mechanism.

Message reads preserve the newest 10,000 fleet-send window, but existing endpoint
indexes select the owner's sends before any payload decoding. Two partial indexes
retain legacy sends whose endpoint fields are at the body root. Replay additionally
selects only sends after its cursor and before the captured claim frontier. Runtime
membership reads indexed acceptance times instead of decoding every observation
into Rust. Message payloads are copied in batches of at most 64 rows, stopping at
1 MiB plus one complete row; each SQL statement and connection is released before
JSON decoding. No encompassing SQLite transaction spans native I/O, preparation,
payload decoding or long-poll waiting. A concurrent-checkpoint test pauses decoding
a 2 MiB message and proves a subsequent write can be fully checkpointed.

The isolated before/after changes-handler measurement uses a consistent 10.55 GB
store copy with 784,162 retained claims (frontier 960,761), a representative agent
with 19,339 claims, and a fixed 128-record native fixture. Four warmed debug-build
samples append one native reply and one harness observation, request negotiated
blocks, and use zero long-poll wait. Both variants return two items and 1,006 bytes.
Handler time changes from 781–787 ms to 246–277 ms; CPU from 781–787 ms to 244–269 ms;
SQLite VM work from about 343,000 steps to 118,500. The longest SQL statement/reader
lifetime drops from 229–231 ms to 4–5 ms. Statement count rises from 103–105 to
117–119 because payload copies use short bounded batches. These measurements
isolate graph work and do not reproduce the full production 11-second read or
identify its WAL holder. Owned-message processing and native window preparation
remain separate costs; the incremental native-window/page slice is #1665.

The two legacy indexes add no payload class, claim kind, retention rule, client
contract or projection digest. Their first builds on the isolated store took
17.08 seconds and 0.34 seconds (the second reused warmed pages); subsequent opens
reuse them. This startup cost belongs in rollout planning. Older builds ignore
the additional indexes. The routing cache is cleared with projection repairs and
rebuilds after restart.

## Stored session timeline graph reads

Until the owner-native reader cutover removes the stored-history fallback, its
attribution reads only the desired row for the session owner by primary key.
The owner record query first selects the existing newest 10,000 physical owner
rows, then selects the kinds that the fallback already renders: runtime and
harness status, diagnostics, usage and messages. It does not filter kinds before
the physical window, which would bring older entries back into the view. Timeline
operations keep their separate incarnation window and missing-history checks.

Selected owner payloads use the same 64-row, 1 MiB-plus-one-complete-row copy
batches as messages. Statements and connections are released before JSON decoding;
a paused 2 MiB diagnostic decode allows a concurrent write to be fully checkpointed.
Existing subject and claim identity indexes suffice. This cuts unnecessary payload
copies and the long decode-held reader, without changing stored data, retention,
visible entries, attribution or cursors. The operation fold, native normalization,
response preparation and serialization remain separate costs; this slice alone
does not establish the client p99 or attached-client CPU targets.

Existing request profiles distinguish `timeline/native-read`,
`timeline/stored-fallback` and `timeline/cached-page` stages so deployment evidence
can identify the path beneath the shared session-detail route label. The native
incremental fold and bounded prepared pages remain #1665's scope. Session detail
and session-list collection construction remain separate from timeline reads.

### Local collection OPEN and idle rechecks

The terminal UI (`st3-feed`) and phone (`apps/ios/feed.ts`) subscribe to a conversation on
one existing collections socket. The first `replace:true` frame contains the newest 200
entries, subject to the existing 1 MiB frame limit, with `has_more` when older content exists.
Older content remains available through the existing 200-entry timeline pages and owner
content chunks. Subsequent `replace:false` frames carry new or revised entries by ID.

For a local owner, the first page supplies its own native replay boundary. The local-observation frontier precedes the graph snapshot; both precede that page, so a
durable message or local row committed during the read remains replayable afterward. OPEN no longer builds a second full native timeline
solely to obtain an empty baseline cursor. Remote owners retain the existing two-read relay
protocol for mixed-build compatibility.

Idle timer and graph wakes test indexed owner/endpoint/local-position metadata, without
copying and decoding unrelated claim bodies. The query seeks both wrapped and legacy
message endpoint indexes and the local subject/position index at captured frontiers.
The CI work budgets pin one initial timeline build, an appended native record’s entries as a delta for
short and long fixtures, concurrent-message replay, and fewer than 250 SQLite VM steps
for an absent owner/endpoint wake at 100 and 10,000 unrelated sends.

This cut adds no wire field, schema, index, payload class or retention rule. It does not
establish a production latency percentile: request-to-first-full-frame and reactive-update
measurements must disclose client, source, payload, window and observed population.
The native incremental fold and selective entry preparation are supplied by #1665 on main.
This OPEN cut uses that read path for its one page; it adds no second native cache and
leaves the owner fold and bounded preparation unchanged.
