# Client v0 turn changes proposal

Status: proposed. Review: Nathan. No implementation changes in this document.

This extends the [client contract](README.md) for per-turn changed files, patches, and a branch-wide **Changes** scope in a web client.
The schema and fixtures remain normative until implementation PRs change them; this is a design-doc proposal, not a filed issue or ratified requirement.

## Current boundary and evidence

- Owner-native transcripts preserve generic tool calls/results, not an authoritative file-change ledger: Codex function/custom tools in `crates/st3/src/external_sessions.rs:2866–2884`, Claude `tool_use` in `:3003–3017`, and OMP `toolCall` in `:3230–3251`; names, arguments, and call IDs survive (`:3320–3337`). Edit/write tools therefore provide provenance when present, not complete mutation coverage.
- The older producer path normalizes Claude pre/post-tool hooks (`crates/st-drivers/src/harness_timeline.rs:548–554`) and OMP channel calls/results (`:907–921`); its arguments/results are digests rather than patches (`:1144–1174`). OMP's hook also redacts these event bodies (`crates/st-drivers/hooks/omp-channel.ts:503–511`). Do not confuse these records with owner-native transcript content.
- Codex producer item handling includes command/MCP calls, but no dedicated file-change arm (`crates/st-drivers/src/harness_timeline.rs:483–531`). Neither successful edit events nor shell command text prove the final filesystem state.
- `Agent.workspace` and `Agent.checkout {repository, base, branch}` already exist; checkout describes requested state, not successful realization (`docs/st3/client-v0/schemas/client-v0.schema.json:1048–1055`).
- The workspace read is declaration-derived and host-scoped (`crates/st3/src/api/client_v0.rs:851–895`). Repository discovery inspects the workspace and ancestors, not nested repositories or arbitrary directories (`crates/st3/src/repositories.rs:49–70`).
- Reconciliation creates/validates worktrees only on their declared owner (`crates/st3/src/reconcile.rs:2010–2034`), and records workspace/repository observations (`:4426–4439`). Checkout handles lifecycle, not diff projection (`crates/st3/src/checkout.rs:82–168`).
- The daemon can spawn local Git: checkout uses `git -C repository`, disabled prompts, and deadlines (`crates/st3/src/checkout.rs:171–205`). This establishes local execution capability, not an existing workspace-diff API; its 30-second lifecycle timeout is unsuitable here (`:15–17`).
- Conversations already route reads to the session owner with delegated authority and explicit unavailability (`crates/st3/src/api/client_v0.rs:360–423`). A gateway's filesystem is not the remote agent's workspace.
- Timeline entries support append/replace/finalize revisions, generic tool bodies, and usage-local `turn_id`, but no typed changes summary or patch read (`docs/st3/client-v0/schemas/client-v0.schema.json:1645–1689`). A targeted daemon/schema search found no Git status/numstat/turn-diff projection; existing `diff` fields describe graph launch previews.

Sources above are repository-relative file:line citations; the relevant files are [native normalization](../../../crates/st3/src/external_sessions.rs), [producer events](../../../crates/st-drivers/src/harness_timeline.rs), [checkout](../../../crates/st3/src/checkout.rs), [owner routing](../../../crates/st3/src/api/client_v0.rs), and [schema](schemas/client-v0.schema.json).

## Source of truth and turn identity

Use owner-daemon snapshots of repository content at turn boundaries; derive summaries and patches from the same immutable snapshot pair.
Harness events identify boundaries and explain tools, but never supply authoritative changed-file counts: shell scripts, formatters, nested tools, failed partial writes, and Git operations bypass edit/write accounting.
Capture the working tree including tracked files and eligible nonignored untracked files, plus HEAD/index metadata; HEAD-to-HEAD alone misses uncommitted work.
Use a daemon-private object store and temporary index, without modifying the user's index, branches, stash, or worktree; capture raw content without filters, hooks, external diff, or textconv.
Write worktree and index snapshots to private trees/refs with `git write-tree` over a temporary index (equivalent in purpose to `git stash create`), reusing prior blobs and updating dirty entries without touching the user's index or HEAD, so warm content capture is O(changed files), not a workspace copy; initial seeding and metadata scans remain separately bounded.
A driver must establish the start snapshot before releasing a new turn to mutate files; a late observation/import cannot reconstruct the missing baseline and remains Unknown.
At authoritative completion, failure, or cancellation, capture the end before releasing the successor; retain partial edits on cancelled/failed turns.
While a turn runs, publish start-to-now **provisional** pairs: owner worktree/index/HEAD invalidations and tool-batch completion trigger a coalesced private-tree capture, at most twice/second/root with one in flight; a once/second bounded reconciliation while subscribed covers missed notifications and shell-only writes.
Each publication increments the turn entry's revision (mirrored by its body revision) and fully replaces its file set/totals, never adds deltas; edit/revert removes the file from the replacement. Discard superseded provisional work; final endpoint capture bypasses provisional debounce, not worker/size bounds.
Mark `phase: provisional` and `final: false`; only the settled endpoint pair becomes `phase: complete | interrupted`, `final: true`. Until a usable baseline/current capture exists, live state remains Unknown.
Persist snapshot identities and bounded content on the owner, not patch bodies in replicated claims; daemon projections are derived and client state is only a subscribed view.

Reuse the canonical `turn_id` proposed by [#1824](https://github.com/compoundingtech/smalltalk/pull/1824), bound to conversation/session, runtime incarnation, owner generation, and workspace declaration.
Map native turn IDs once in the driver; no client-generated IDs, timestamps, message text, tool IDs, or independent changes-turn counter.
Queued messages do not create turns until consumed; steering inside an active turn retains that turn's identity.
Start/end snapshot refs survive daemon restart within retention; an interrupted turn lacking a provable endpoint is Unknown, never silently finalized or assigned to its successor.

Correctness and scope:
- A turn summary is **net content change between captured states**, not every edit operation: edit/revert yields zero; two tools touching one file yield one net entry.
- A commit within a turn does not erase its content change; commit-only movement with identical working-tree content adds no text delta. Staging-only movement is index metadata, not invented line changes.
- Pre-existing dirty files belong in the baseline: only their additional net changes belong to that turn. Background writes after the endpoint fall outside it.
- Changes observed during the interval are not proof of authorship. Shared workspaces/external writers require an explicit `attribution: interval`, and `consistency: writer_fenced | best_effort`; unstable captures are Unknown/partial, not exact claims.
- Support multiple explicitly registered repository roots with independent snapshot pairs and opaque `repository_id`s. No recursive host scan; newly discovered roots without a start snapshot are Unknown. Out-of-scope writes are unsupported, not zero.
- Submodules appear as gitlink changes; their working files require a separately registered root. Non-Git roots are Unsupported; detached HEAD can support turn diffs but not a declared-branch Changes scope without an explicit base.

## Proposed client v0 contract

Advertise optional `conversation.turn-changes` and `agent.changes` capabilities; absent advertisement is Unknown, not proof of Unsupported. Add `changes_support: {state, reason, roots: [{repository_id, state, reason}]}` to timeline HTTP pages and every conversation stream/changes response, including the initial empty frame; conversation responses also carry nullable `current_turn_id`. Support-only changes emit a frame without inventing a timeline entry; the agent changes read/subscription carries the same scoped support object even with no turn or files.
Add a typed `changes` timeline entry with the existing stable entry ID, sequence, revision, and finalization rules; its body includes:

| Field | Contract |
| --- | --- |
| `turn_id`, `state`, `reason` | Exact canonical turn and `known | unknown | unsupported`; machine-readable reason when not known. |
| `revision`, `phase` | Monotonic revision within the fenced scope; `capturing | provisional | complete | interrupted`, independent of the harness turn's terminal outcome. |
| `repositories` | Root IDs, snapshot refs, coverage/omissions, attribution, consistency, and repository-local file summaries. |
| `files[]` | Stable file ID, repository-relative path, optional old path, add/modify/delete/rename/type-change status, binary flag, additions/deletions, and opaque `patch_ref`. |
| `totals`, `coverage` | File/line totals where known; `complete | partial` and explicit omitted counts/reasons. Binary/unavailable line counts are null, never fabricated zeroes. |

Publish capturing/Unknown promptly, then provisional full replacements; finalize only after endpoint capture settlement. Known empty requires complete coverage and a valid pair; no current turn is `current_turn_id: null`, not a fabricated empty turn.
Summaries travel in conversation frames and resumed HTTP pages, never patch bytes. Sort roots bytewise by `repository_id`, and files by `(repository_id, path, old_path-or-empty, file_id)` using UTF-8 byte order and stable IDs as tie-breakers.
At 200 files or the frame byte bound, return an opaque `summary_ref` and `next_cursor`; `changes-summary.page` is `GET /v1/client/changes/summaries/{reference}?cursor=CURSOR`, returning `{binding, revision, snapshot_pairs, files, next_cursor}`. Null cursor ends that immutable file set.
Continuation refs bind scope, revision, every root's snapshot pair, authorization and last sort tuple; clients merge pages only with identical bindings/revision, then atomically replace the prior file tree. Superseding an active/provisional revision invalidates its refs; settled historical revisions remain readable until expiry or binding revocation.

### Patch fetch and typed outcomes

`changes-patch.chunk` is `GET /v1/client/changes/patches/{reference}/chunk?cursor=CURSOR`; omit cursor for the first chunk. Refs/cursors are opaque and owner-routed; callers supply no paths or Git arguments.
Every response carries `kind: changes-patch-chunk`, `binding`, `repository_id`, `file_id`, `revision`, `from_snapshot`, `to_snapshot`, `variant`, `metadata`, `media_type`, `offset`, `size`, base64 `data`, nullable `next_cursor`, and `truncation: null | {reason, omitted_bytes}` (unknown omitted length is null). Metadata supplies `old_path`, `new_path`, `old_mode`, `new_mode`, `old_oid`, `new_oid`, or `reason` where applicable; media type is `text/x-diff; charset=utf-8` for unified patches, otherwise null.
`binding` is discriminated: `{kind: turn, conversation_id, turn_id, incarnation_id, owner_generation, workspace_token}` or `{kind: agent, agent_id, owner_generation, workspace_token, section}`; agent `section` is `combined | committed | staged | unstaged | untracked`. Branch scope never invents a turn ID.
For `variant: unified`, decoded bytes are UTF-8 `text/x-diff`: Git-style file headers plus `---`/`+++` paths, `@@ -old_start,old_count +new_start,new_count @@` hunks, space/plus/minus line prefixes, and explicit no-final-newline markers; old/new coordinates support unified or split rendering.
`rename_only` has old/new path and mode metadata with no hunks; `binary` has metadata and no text bytes; `gitlink` carries old/new object IDs; `unavailable` carries a typed content/capture reason and no bytes; `metadata_only` represents mode/type-only changes with an explicit reason. Non-text encodings are binary, not lossy UTF-8.
Chunks are ≤256 KiB decoded bytes and reconstruct the retained patch prefix; `size` is its retained byte length. The 2 MiB per-file patch cap stops at a complete hunk boundary and sets `truncation.reason: patch-size-cap`; even when no hunk fits, retain metadata and that marker. A nonnull cursor means another retained chunk, not truncation; the last capped chunk has null cursor and still reports truncation.
Use the existing `st3.client.error.v0` envelope (`api_version`, `request_id`, `code`, `message`, `retryable`, `details`, optional `retry_after_ms`) for failed reads, never a successful empty patch. Successful support/summary Unknown values are projections, not error envelopes.

| Outcome | HTTP/projection or error | Retry/recovery |
| --- | --- | --- |
| Known conversation/root/agent support | 200 `changes_support.state: known`; each summary separately reports content state/coverage. | Render only known counts; no turn means no current-turn card. |
| Unknown support or capture | 200 `state: unknown`, reason such as missing baseline, capturing, or unknown adapter; per-root omissions remain explicit. | Await a support/revision update; never infer zero. |
| Unsupported conversation/root/agent | 200 `state: unsupported` with reason; an unsupported fetch is 409 `unsupported-capability`. | Fetch error `retryable: false`; no fallback parser. |
| Permission denied/revoked | 403 `forbidden`; no content or successful Unknown masquerading as permission. | `retryable: false`; reacquire authorization explicitly. |
| Owner lost | Support update Unknown/`owner-unavailable`; owner-dependent reads fail 503 `remote-unavailable`, `details.availability: owner-unavailable`. | `retryable: true`; back off, do not read gateway files. |
| Deadline/busy | 504 `read-deadline` / 429 `rate-limited`, with scoped details and bounded `retry_after_ms` for busy. | `retryable: true`; back off, retain last view labelled noncurrent. |
| Retained bytes/ref expired | 410 `changes-content-expired`, `details.ref_kind: patch | summary`, `full_resync: false`. | `retryable: false`; history is expired, reload cannot recover evicted bytes. |
| Active revision/binding invalidated | 410 `changes-content-invalidated`, `details.full_resync: true`. | `retryable: true` only after reloading scope and obtaining new refs; never retry the stale ref. |

Clients use typed states/codes, apply only matching revisions, drop refs on reset/replacement/revocation, and refetch after invalidation; no persistent client Git cache or transcript-derived fallback.

### Branch/working-tree Changes: committed and uncommitted

Add `GET /v1/client/agents/{agent}/changes` and a corresponding declared-interest subscription using the existing projection/collection conventions.
Resolve the declared base to `base_sha` once when activating the workspace binding; do not fetch or let a moving remote-tracking ref silently change the scope.
Return `base_sha`, `head_sha`, `merge_base_sha`, branch/workspace binding, observation revision/time, and separate committed, staged, unstaged, and eligible-untracked sections.
Committed scope is commits reachable from HEAD but not the pinned base (`base_sha..HEAD`); its net diff uses `merge_base(base_sha, HEAD)..HEAD`.
The combined files/patch view compares that merge-base tree with captured working-tree content, including untracked additions; it is not the sum of committed and dirty line counts.
The selector is **Current turn / Branch/working tree**, not **All turns**: pre-existing dirty files may appear only in branch scope, and cross-turn edit/revert may vanish there. No all-turn aggregate is proposed; the client never sums turn counts or relabels this scope as All.
Keep staged/unstaged sections so opposing index/worktree changes remain visible even when their combined content delta is zero.
Commit lists and summary files are paginated; a clean worktree with branch commits still has Changes. Unresolved base/unrelated history is Unknown for committed scope, not silently HEAD-only.
Branch switches/rebase/reset advance the revision and invalidate active scope refs; historical turn snapshot pairs remain immutable. Base refresh is explicit, not a background reinterpretation.

## Remote ownership, speed, and size/privacy

Run capture and Git diff on the workspace owner; route summaries/patches through authenticated client relay reads and fence session/workspace/owner generation.
A remote owner loss changes scoped support to Unknown/`owner-unavailable`, while owner-dependent reads return the error above; never run Git against a coincidentally matching gateway path or present a replica as current authority.
Reuse bounded relay cancellation from [#1826](https://github.com/compoundingtech/smalltalk/pull/1826); relocation requires snapshot continuity or explicit expired/Unknown history.
Proposed measurable budgets, not measurements: warm summary read/serialization p95 ≤16 ms; capturing frame ≤100 ms after boundary; complete bounded summary p95 ≤100 ms after endpoint capture.
Bounded patch fetch p95 <300 ms end-to-end on a reachable owner, including a one-hop relay. Benchmark cold and warm paths separately; deadline exhaustion is a typed `read-deadline` error, not a successful Unknown patch, stalled frame, or partial success.
Git/snapshot work runs outside frame serialization, the Store writer, and the lifecycle reconciler; coalesce per-root invalidations, stream/drain subprocess output, and bound worker slots/queues with cancellation and explicit busy responses.
Suggested review limits: ≤64 KiB summary per frame (and existing 1 MB enclosing page), 200 files/page, 256 KiB patch chunks, 2 MiB text patch/file, 8 MiB captured file, 64 MiB capture/root, four expensive owner operations.
Over-limit/binary files keep bounded metadata and explicit unavailable/truncated patch markers; omitted capture content cannot supply exact historical line counts. Large repositories must not block other agents.
Propose owner-local retention of seven days with a 256 MiB per-workspace content cap; deterministic eviction keeps small summaries with expired patch state. Pin in-flight pairs only within the same bound; deletion/revocation releases content.
Reuse `read.projections` authorization only if Nathan approves that it extends from transcript content to captured workspace files; a declared workspace path alone grants no arbitrary file read.
Capture registered roots only; exclude Git metadata and ignored untracked files, never follow symlinks outside roots, and expose relative paths/opaque IDs rather than absolute source paths.
No secret/token scrubbing of admitted patch content, consistent with [#1561](https://github.com/compoundingtech/smalltalk/issues/1561); authorization, explicit capture scope, passive text rendering, and size limits are the privacy boundary.
Unlike today's native-conversation reads, this proposal adds owner-local historical file capture; approve retention and access explicitly rather than treating it as already authorized behavior.

## Overlap and implementation split

Searched open issues and PRs with `gh ... list --repo=compoundingtech/smalltalk --search` for diff/file changes/git status/turn_id and exact turn-diff/changed-files/numstat terms; no dedicated end-to-end turn-diff contract was found.
- [#705](https://github.com/compoundingtech/smalltalk/issues/705) requests workspace projection for changed-file browsing: reuse the now-present host-scoped Agent fields; do not duplicate workspace discovery.
- [#1824](https://github.com/compoundingtech/smalltalk/pull/1824) proposes exact-turn identity/cancellation: share its turn lifecycle and fences.
- [#1313](https://github.com/compoundingtech/smalltalk/issues/1313) covers typed harness state; [#933](https://github.com/compoundingtech/smalltalk/issues/933) covers declared interests/shared projections: compose support state and subscriptions, not another parser/poller.
- [#1665](https://github.com/compoundingtech/smalltalk/pull/1665) incrementally folds native conversations; [#1561](https://github.com/compoundingtech/smalltalk/issues/1561) sets owner-content/no-scrubbing policy; [#1826](https://github.com/compoundingtech/smalltalk/pull/1826) bounds forwarded reads. None establishes historical Git snapshots.

**Daemon/drivers:** authoritative turn admission/end capture, root registry, owner-local snapshot lifecycle, Git summaries/patches, invalidation, authenticated relay, tri-state errors, and bounded shared projections. Nathan reviews schema/operation manifest/capabilities/fixtures and contract tests before implementation.
**SDK/client:** regenerate Rust/Swift/TypeScript from that contract; expose typed reads, refs, frames, and errors. A web client renders per-turn files/counts, lazy patches, and branch Changes, with explicit Unknown/Unsupported/truncation; no client Git or independent stale cache.
**Acceptance scenarios (proposed, not executed):** edit/write and shell-only edits agree; provisional edit/revert removes the file under a higher revision; cancelled/failed turns settle partial work without affecting successors. Pre-existing dirty work, commit-during-turn, opposing staged/unstaged edits, clean committed branches and cross-turn reverts distinguish Current turn from Branch/working tree. Exercise multiple roots, submodules, rename-only/delete/binary/gitlink/untracked/ignored files, >200-file continuations racing publication, unified/split hunk coordinates, chunk completion versus cap truncation, missing boundaries, concurrent writers, restart, rebase/base movement, owner loss/relocation, denied/revoked access, busy/deadline and expired versus invalidated refs. Unknown/Unsupported must render without an entry and never masquerade as complete empty results; verify cold/warm budgets under multi-agent load and resumed frames without fetching patches.

## Questions for Nathan

- **DQ1 Boundaries and identity:** approve reuse of #1824 `turn_id` and capture-before-release driver admission? Which adapters can prove endpoints, including interrupted/background work?
- **DQ2 Meaning and consistency:** approve net interval content rather than tool-authored edits, with disclosed best-effort shared-workspace consistency and separate index metadata?
- **DQ3 Roots and Changes base:** approve explicitly registered multi-repo roots and pinned-base/merge-base semantics, including detached/unrelated history and explicit base refresh?
- **DQ4 Contract and access:** approve timeline changes entries plus owner patch reads/agent scope subscriptions, and existing `read.projections` versus a narrower workspace-content grant?
- **DQ5 Capture/retention/performance:** approve owner-local snapshot storage, expiry/limits, and the proposed budgets; should historical patches live seven days or another explicit horizon?
