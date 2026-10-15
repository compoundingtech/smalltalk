# Conversation timeline blocks

st shows in a conversation what the harness shows in its terminal, normalized for
the terminal UI, web and phone without token filtering, secret scrubbing or allow-lists.
Access checks still apply. Only reasoning the harness exposes is shown.

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
Errors also have a known `error` block with `{body_ref:true}` for native stop/exit
notices. Optional `block.metadata` is an open JSON
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

The normalizer in `crates/st3/src/external_sessions.rs` retains exposed reasoning and
image references. A 512-character unknown excerpt is a UI preference only. Native
transcript formats remain authoritative.

## Typed views and conversation header

Typed views and the header are additive and optional, on top of the block contract, so an
old client ignores the new fields and keeps the existing text fallback. A block MAY
carry `view: {type: <string>, ...}`; `type` is an open discriminator, and the client-v0
schema encodes one closed definition per known `view.type` (`TimelineView*` in
`docs/st3/client-v0/schemas/client-v0.schema.json`) plus a `TimelineView` fallback
branch that accepts any other type string. A renderer that does not know a type
renders the block as it does a block without a view. `view` holds parsed fields only;
the full native arguments or output stay in `payload`, without changing the block contract. Raw JSON
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
arrives and draw all assignment lines only on the invocation's expansion. Subagent
result cards show status, usage and the child-open link, without repeating the
assignment. The child-open row names the agent while routing to its actual session
id. The owner fills
`conversation.session_id` only when the child transcript exists at
`<parent transcript without .jsonl>/<id>.jsonl` and is readable; that session id opens
through the normal conversation routes (same fold, paging and refs), which is how a
subagent card links to the child transcript as its own conversation.
`JobSummary` = `{id, name?, type?, state, exit_code?, started_at?, ended_at?,
duration_ms?, output_bytes?}`.

Extension and bookkeeping records become blocks as follows; anything not listed keeps
its block shape (other `custom_message` records with display != false stay `text`):

| OMP record | block kind | view |
| --- | --- | --- |
| custom_message `irc:incoming` | `irc` | `{type:"irc", from, message, reply_to?, message_id}` |
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
Simplified rendering and history auto-fill share the same bundling predicate:
write, compaction and assistant-error preview cards remain separate rather than
being counted as one ordinary-tool run.


Timeline pages and delta responses (when `conversation-blocks.v1` is negotiated) MAY
carry a `header` object. Each field is `{value, source: register|transcript, as_of}`:
`model` (model id), `context` ({tokens, window}), `cost` ({usd}, with `window: true`
when the fold window is truncated), `todos`, `jobs` ([JobSummary]), `subagents`
([SubagentSummary]), `ask` (the last ask call without a matching result, or null) and
`working` (bool). `source` can be `register` or `transcript`; a register field takes
precedence when supplied. The current `conversation_header::register_value` adapter
returns `None`, so headers contain transcript-derived fields and omit `working`. A
field is absent when its source supplies no value. Transcript derivation uses the
normalized block metadata: model = last `metadata.model`; context = last
`metadata.context_tokens`; cost = sum of `metadata.usage.cost_usd` in the window;
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
control-character, internal-block and excerpt filters. This keeps terminal UI and the
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

The terminal UI and the phone fetch image bytes and clipped remainders through the
chunk route. Existing graph-message image handling is separate. Generated Rust,
Swift and TypeScript clients expose the chunk API.

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

The 8 KiB/value and 768 KiB timeline-page bounds protect memory and transport only.
The initial chunk contract returns up to 256 KiB decoded bytes; image reads have
a visible 32 MiB decoded limit. Four expensive owner timeline/chunk reads can run
concurrently; excess reads, including managed timeline reads, return HTTP 429
`rate-limited`, without a queued backlog. Clients must back off and retry.
Input bounds are distinct from response clipping. A JSONL read captures at most
32 MiB of source bytes and retains at most 4,096 complete lines from that window.
The partial first line and older prefix are marked **not fetchable through this
owner read**; there is no prefix continuation. A line exceeding the
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
these do not impose a hard heap/AST budget or guarantee streaming JSON normalization.

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
Paging cursors fence source revisions; invalidation requests resync and preserves
explicit notices for unavailable content.

## Client compatibility

Existing terminal UI/iOS clients retain their negotiated capabilities. Swift decodes a closed
`TimelineType` enum, so sending new top-level kinds to old clients is breaking.
Advertise/negotiate blocks and chunk fetch; retain existing entry types and complete
text/JSON fallbacks with explicit transport truncation notices for legacy clients.
New clients render reasoning, full expandable arguments/results, unknown JSON,
images, jobs/subagents, asks and availability consistently. Old clients keep their
known text/tool/status behavior; they do not gain image fetch or new interactive
rendering. An incompatible wire change requires rejecting an old client.

## Collection timeline frames

The terminal UI (`st3-feed`) and phone (`apps/ios/feed.ts`) subscribe to a conversation on
one existing collections socket. The first `replace:true` frame contains the newest 200
entries, subject to the existing 1 MiB frame limit, with `has_more` when older content exists.
Older content remains available through the existing 200-entry timeline pages and owner
content chunks. Subsequent `replace:false` frames carry new or revised entries by ID.

Native assistant stop outcomes use the shared `assistant_error` block and harness
presentation rules: superseded attempts, silent aborts and user interrupts render
nothing; recovered attempts show a compact note and terminal failures render in full.
The view retains the exact native `stopReason`, `errorMessage`, `retryRecovery`,
`errorStatus` and `errorId` under `native`, alongside the presentation projection.
Native source records remain available through owner reads.

Session exits preserve the complete checkpoint, including `pendingToolCalls`,
arguments/intent and unknown fields. Only transport size and paging limits apply,
with visible truncation markers and owner continuations for exact full values.
Outcomes are read from the owner machine, not stored in the database.
Recognized exits use the existing `cancelled` status value — the process stopped
before the turn finished; the recorded kind, reason and exit code stay in `detail` —
so a planned stop is neither a completed turn nor an error, with no new
client-visible vocabulary. Fatal and unknown exits remain diagnostic errors.
