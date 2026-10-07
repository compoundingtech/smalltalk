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

Advertise optional `conversation.turn-changes` and `agent.changes` capabilities, plus scoped support state; absent advertisement is Unknown, not proof of Unsupported.
Add a typed `changes` timeline entry with the existing stable entry ID, sequence, revision, and finalization rules; its body includes:

| Field | Contract |
| --- | --- |
| `turn_id`, `state`, `reason` | Exact canonical turn and `known | unknown | unsupported`; machine-readable reason when not known. |
| `revision`, `phase` | Snapshot-pair revision; `capturing | complete | interrupted`, independent of the harness turn's terminal outcome. |
| `repositories` | Root IDs, snapshot refs, coverage/omissions, attribution, consistency, and repository-local file summaries. |
| `files[]` | Stable file ID, repository-relative path, optional old path, add/modify/delete/rename/type-change status, binary flag, additions/deletions, and opaque `patch_ref`. |
| `totals`, `coverage` | File/line totals where known; `complete | partial` and explicit omitted counts/reasons. Binary/unavailable line counts are null, never fabricated zeroes. |

Publish a capturing/Unknown entry promptly, replace it when the immutable pair is ready, and finalize only after capture settlement.
Known empty requires complete coverage and a valid baseline/end; an interrupted capture may finalize as Unknown without changing the turn's cancelled/failed status.
Summaries travel in conversation frames and resumed HTTP pages; bounded overflow uses a summary continuation ref. No patch bytes in frames.
Add an owner-routed, read-only patch fetch by opaque `patch_ref` and bounded offset/cursor, not a caller-provided filesystem path or Git arguments.
Responses bind repository, turn, summary revision, and snapshot pair; return media type, content, byte range, completion state, and typed truncation/omission markers.
References authorize one captured pair and file; expiry is `changes-content-expired`, stale binding is `changes-content-invalidated`, and permission denial is not a successful empty patch.
Clients render server state and revisions, drop refs on reset/replacement/revocation, and refetch after invalidation; no persistent client Git cache or transcript-derived fallback.

### Changes scope: committed and uncommitted

Add `GET /v1/client/agents/{agent}/changes` and a corresponding declared-interest subscription using the existing projection/collection conventions.
Resolve the declared base to `base_sha` once when activating the workspace binding; do not fetch or let a moving remote-tracking ref silently change the scope.
Return `base_sha`, `head_sha`, `merge_base_sha`, branch/workspace binding, observation revision/time, and separate committed, staged, unstaged, and eligible-untracked sections.
Committed scope is commits reachable from HEAD but not the pinned base (`base_sha..HEAD`); its net diff uses `merge_base(base_sha, HEAD)..HEAD`.
The combined files/patch view compares that merge-base tree with captured working-tree content, including untracked additions; it is not the sum of committed and dirty line counts.
Keep staged/unstaged sections so opposing index/worktree changes remain visible even when their combined content delta is zero.
Commit lists and summary files are paginated; a clean worktree with branch commits still has Changes. Unresolved base/unrelated history is Unknown for committed scope, not silently HEAD-only.
Branch switches/rebase/reset advance the revision and invalidate active scope refs; historical turn snapshot pairs remain immutable. Base refresh is explicit, not a background reinterpretation.

## Remote ownership, speed, and size/privacy

Run capture and Git diff on the workspace owner; route summaries/patches through authenticated client relay reads and fence session/workspace/owner generation.
A remote owner loss returns Unknown/`owner-unavailable`; never run Git against a coincidentally matching gateway path or present a replica as current authority.
Reuse bounded relay cancellation from [#1826](https://github.com/compoundingtech/smalltalk/pull/1826); relocation requires snapshot continuity or explicit expired/Unknown history.
Proposed measurable budgets, not measurements: warm summary read/serialization p95 ≤16 ms; capturing frame ≤100 ms after boundary; complete bounded summary p95 ≤100 ms after endpoint capture.
Bounded patch fetch p95 <300 ms end-to-end on a reachable owner, including a one-hop relay. Benchmark cold and warm paths separately; deadline exhaustion is typed Unknown/read-deadline, not a stalled frame or partial success.
Git/snapshot work runs outside frame serialization, the Store writer, and the lifecycle reconciler; coalesce per-root invalidations, stream/drain subprocess output, and bound worker slots/queues with cancellation and explicit busy responses.
Suggested review limits: ≤64 KiB summary per frame (and existing 1 MB enclosing page), 200 files/page, 256 KiB patch chunks, 2 MiB total text patch, 8 MiB captured file, 64 MiB capture/root, four expensive owner operations.
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
**Acceptance scenarios (proposed, not executed):** edit/write and shell-only edits agree; edit/revert, partial failure/cancel, pre-existing dirty work, commit-during-turn, staged/unstaged cancellation, and clean committed branches have correct net scope; multiple roots, submodules, rename/delete/binary/untracked/ignored files are explicit; missing boundaries, concurrent writers, restart, rebase/base movement, owner loss/relocation, revoked access, oversized files and expired refs never masquerade as complete empty results. Verify budgets under cold/warm multi-agent load and resumed frames without fetching patches.

## Questions for Nathan

- **DQ1 Boundaries and identity:** approve reuse of #1824 `turn_id` and capture-before-release driver admission? Which adapters can prove endpoints, including interrupted/background work?
- **DQ2 Meaning and consistency:** approve net interval content rather than tool-authored edits, with disclosed best-effort shared-workspace consistency and separate index metadata?
- **DQ3 Roots and Changes base:** approve explicitly registered multi-repo roots and pinned-base/merge-base semantics, including detached/unrelated history and explicit base refresh?
- **DQ4 Contract and access:** approve timeline changes entries plus owner patch reads/agent scope subscriptions, and existing `read.projections` versus a narrower workspace-content grant?
- **DQ5 Capture/retention/performance:** approve owner-local snapshot storage, expiry/limits, and the proposed budgets; should historical patches live seven days or another explicit horizon?
