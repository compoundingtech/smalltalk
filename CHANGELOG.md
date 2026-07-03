# Changelog

All notable changes to `@myobie/coord` (renaming → `@myobie/smalltalk`) are
recorded here. The project is pre-1.0; expect breaking changes in
minor releases until 1.0.

## Unreleased

### Added (brief-022 — `st launch --persona <path>` surgical persona linking)

`st launch <harness>` now optionally installs a persona alongside the
harness bootstrap. Given `--persona <path>`:

- Copies the source file to `<cwd>/PERSONA.md`.
- Surgically edits the harness entry file — `CLAUDE.md` (claude) or
  `AGENTS.md` (codex) — to append `@PERSONA.md` on its own line. If the
  file already exists, its content is preserved byte-for-byte; only
  the import line is added, and only when not already present.
- Git-excludes `PERSONA.md`, `.mcp.json`, `.claude-session-id`,
  `.codex-session-id`, `pty.toml`, and the entry file (only when we
  created it) via `.git/info/exclude`. A pre-existing repo CLAUDE.md
  is never added to the ignore list.
- `--dry-run` prints the exact plan (copy target, entry-file action,
  git-exclude entries) without touching disk.

Verified 2026-07-02 that codex 0.142.4 honors `@PERSONA.md` in
`AGENTS.md` (empirical test: agent replied with persona-defined value
when asked; directory listing in the transcript showed `AGENTS.md` and
`PERSONA.md` were both read). Both harnesses use the same mechanism —
same code path in launch.ts, only the entry-file name differs.

12 new unit tests cover: copy target, entry-file create-vs-append,
idempotence (re-append is a no-op), trailing-newline-less existing
files, git-exclude entries + dedup, entry-file exclusion decision
(never for pre-existing files), non-git-repo warning, missing source
error, and dry-run planning.

### Added (brief-rename-cutover — Phase C sweep scripts under `tools/cutover/`)

Testable rewriters + a directory-walking driver for the per-machine
step of the coord→st cutover. All pure functions on the file text
(idempotent by construction) with a thin CLI driver that walks disk
and writes back atomically (tmp+rename, `<name>.pre-cutover`
backups on first change).

- **`tools/cutover/rewrite-mcp-json.ts`** — pure rewriter for
  `.mcp.json`. Renames `mcpServers.coord` → `mcpServers.st`,
  flips every legal `coord`-bin path suffix
  (`myobie/coord/bin/coord`, `myobie/smalltalk/bin/coord`,
  `myobie/coord/bin/st`, `myobie/coord/bin/smalltalk`) to
  `myobie/smalltalk/bin/st`, renames `env.COORD_IDENTITY` →
  `env.ST_AGENT` (drops when `ST_AGENT` already set), same for
  `COORD_ROOT` / `COORD_CONFIG`.
- **`tools/cutover/rewrite-pty-toml.ts`** — pure rewriter for
  `pty.toml`. Line-based sweep (no full TOML round-trip →
  preserves formatting, comments, blank lines) covering
  `server:coord` → `server:st`, `coord ding` → `st ding` (both
  with word-boundary guards against `server:coord-web` /
  `coord dingus` false-matches), and the same env-var
  rename/drop rules inside `[sessions.<name>.env]` blocks.
- **`tools/cutover/sweep.ts`** — CLI driver.
  `node --experimental-strip-types tools/cutover/sweep.ts <path>...
   [--dry-run] [--kind mcp-json|pty-toml|both] [--depth <n>]
   [--no-backup]`. `--dry-run` audits without touching disk.
  Atomic writes; backups on first change (skip-if-exists so
  re-runs preserve the original pre-cutover snapshot).
- **31 new unit tests** at
  `tests/unit/tools/cutover/rewrite-*.test.ts` covering every rule,
  idempotence, word-boundary guards, malformed input, and the
  belt-and-suspenders "both `ST_AGENT` and `COORD_IDENTITY` set"
  drop case.
- **`tools/cutover/README.md`** documents the rules + usage.

### Added (brief-rename-cutover Phase P1 — `$ST_CONFIG` env var)

Closes the last `COORD_*` env var that had no `ST_*` equivalent, so
the coord→st cutover's Phase D can drop `$COORD_CONFIG` support
without a gap. Mirrors the pattern already used for `ST_ROOT` /
`COORD_ROOT` (brief-005-phase0):

- New `$ST_CONFIG` env var, preferred over `$COORD_CONFIG`. Setting
  both is fine — `$ST_CONFIG` wins.
- New `~/.config/smalltalk/` default location, preferred when it
  exists. Falls back to `~/.config/coord/` when only that exists
  (legacy machine); creates `~/.config/smalltalk/` on a brand-new
  install. Same reasoning as the state-root logic: the warning
  belongs on env vars (the actionable signal), not the dir shape.
- Reading `$COORD_CONFIG` still works and emits the same one-time
  stderr fallback notice used by `$COORD_ROOT` and
  `$COORD_IDENTITY`.
- `stConfigFrom(env)` is the new canonical resolver;
  `coordConfigFrom` is a same-signature deprecated alias. Both
  `stConfig()` and `coordConfig()` — the current-env wrappers —
  work the same way.
- **8 new unit tests** cover the ST-preferred / COORD-fallback /
  dual-dir / brand-new-install / alias-equivalence axes.

### Added (task #118 — `st launch` generates `.claude/settings.local.json`)

`st launch claude` now writes `<cwd>/.claude/settings.local.json`
with all three smalltalk hooks pre-wired to absolute paths under
the shipped `examples/claude-code/hooks/` directory:

- **SessionStart** with `async: true` + `asyncRewake: true` — the
  brief-020 followup. asyncRewake surfaces the hook's stderr as a
  system reminder that triggers a turn, so a boot / `--resume` /
  `/clear` / `/compact` doesn't leave the agent silent with a
  stale status and an unread inbox. Complements the polling
  backstop that shipped in brief-020.
- **PreCompact** (task #33 hook-legs) — stubs `context/now.md`
  when the model hasn't flushed recently, so boot-rehydrate has
  something to inject after compaction.
- **StopFailure** — surfaces API-error wedges to myobie via coord.

Every new claude agent gets these hooks automatically on
`st launch claude`. Behavior:

- **Skip-if-exists.** An existing `.claude/settings.local.json`
  is left alone (user may have hand-tuned it). Delete it to
  re-bootstrap.
- **`--no-hooks` opts out.** For anyone who wants to wire hooks
  by hand, or an eval that doesn't need them.
- **Codex harness skips.** Claude Code hooks are Claude Code
  specific; codex has its own path via `coord ding`.
- **Missing shipped `examples/`.** Soft-skip with a one-line
  stderr notice — launch still succeeds. `examples/claude-code/`
  is now included in the npm `files` list so installs get the
  hook scripts by default.

`--dry-run` prints the resolved `.claude/settings.local.json` path
and the generated JSON body so the operator can audit before spawn.

### Fixed (task #128 — inbox delivers off-format `.md` as "outside messages")

Before this change, a `.md` file dropped into an agent's `inbox/`
that didn't match the canonical `<unix-ms>-<rand6>.md` grammar was
silently ignored: chokidar, the polling backstop, `coord message
ls`, `read`, and `archive` all filtered it out. A collaborator
unfamiliar with the naming convention could send a message and the
recipient would never see it. This closes that silent-miss hole.

- **`src/common.ts`** adds `validOutsideFilename` (safe off-format
  `.md` basenames — rejects path traversal, dotfiles, and
  prefix-sibling attachments of a canonical `.md`) and
  `validDeliverableFilename` (union of canonical + outside).
  `validFilename` and `filenameTimestamp` retain strict LAYOUT-004
  semantics for callers that depend on prefix derivation.
- **`src/types.ts`** adds `asDeliverableFilename` — brand
  constructor that accepts either LAYOUT-004 or safe outside `.md`.
- **`src/mcp/channel-watcher.ts`** now delivers outside `.md` files
  through the same `notifications/claude/channel` pipeline as
  canonical messages, with `from: "outside"` in the meta envelope,
  a `[outside .md — non-canonical filename: <name>]` marker
  prepended to the content, and no thread reconstruction (outside
  files always start a new thread). Seed, chokidar `add`, and the
  polling backstop all accept the broader `isDeliverable` check.
- **`src/commands/{read,archive,ls}.ts`** accept outside filenames.
  `cmdRead` returns the raw file text as body with
  `from: "outside"` and no frontmatter projection (the file's
  claimed sender can't be trusted through an unofficial name).
  `cmdArchive` moves bytes verbatim; `withAttachments` is silently
  coerced off for outside files (no LAYOUT prefix = no sibling
  family). `cmdLs` includes outside files in `matches` with
  `from: "outside"` in `--json`; ts is derived from file mtime.
- **`src/lib.ts`** — `Coord.ls` maps through `asDeliverableFilename`
  so an outside filename doesn't throw at the API boundary;
  `Coord.read` short-circuits outside filenames to a minimal
  `{message: {from: 'outside', body: <text>}, ...}` shape rather
  than reinterpreting untrusted frontmatter.
- **9 new unit tests** cover `validOutsideFilename` edge cases
  (path traversal, dotfiles, sidecar rejection, the legacy 3-segment
  shape). **New integration tests** in `mcp-channel-watcher.test.ts`
  exercise the outside-.md delivery path; existing tests that
  asserted silent-drop are updated to reflect the new "outside"
  semantics.

### Fixed (brief-020 — channel-watcher wake reliability, HB-4)

Idle Claude Code agents sometimes sat on delivered coord messages
without ever surfacing them (evals-claude 56min, smalltalk-claude
also hit it), even though `coord_msg_ls` would show the file. Root
cause: chokidar's FSEvents backend on macOS can silently stop
delivering `add` events on a long-idle process; the notification
never fires and the wake never happens. Claude Code agents relied
solely on the FSEvents-driven channel notification, so any dropped
event meant a wedged inbox.

- **`src/mcp/channel-watcher.ts`** now runs a polling backstop
  alongside chokidar. Every `pollBackstopIntervalMs` (default 15s),
  the watcher scans the inbox dir, dedupes against a `seen: Set`
  shared with chokidar's `add` handler, and enqueues any un-seen
  valid LAYOUT-grammar files through the same notification pipeline.
  Worst-case wake latency is now bounded by the poll interval even
  when FSEvents is fully dead.
- **Seeding.** On startup, `seen` is populated from an initial
  `readdirSync` so a fresh process doesn't replay historical files
  as fresh arrivals — backlog stays the boot ritual's job.
- **Dedup guarantees.** Chokidar `add` and the backstop check-and-add
  atomically against the same `seen` set, so a single file fires
  exactly one notification regardless of which path observed it
  first. Verified by a new integration test that races both paths
  at aggressive intervals.
- **`COORD_CHANNEL_DEBUG=1`** flag opts into one-line stderr
  instrumentation for each chokidar `add`, each poll-backstop
  discovery, and each notification send. Kept off by default so a
  healthy agent's stderr stays quiet; when it happens again, the
  logs distinguish the FSEvents-drop path from a Claude-Code-side
  wake failure without guesswork.
- **6 new integration tests** exercise the backstop in isolation
  (via `chokidarEnabled: false`), confirm chronological ordering
  under the backstop, verify historical files are not replayed on
  startup, confirm non-`.md` files stay ignored (task #128 later
  relaxed this to deliver off-format `.md` as "outside" messages
  while keeping non-`.md` paths ignored), confirm `close()`
  disposes the timer, and race chokidar + backstop to prove the
  dedup path.

### Added (brief-016 — `smalltalk launch <harness>` one-command bootstrap)

New CLI verb: `st launch <claude|codex>` (also `smalltalk launch` /
`coord launch`) that stands up a harness correctly wired to smalltalk
in a single command. Shaped like `ollama launch`.

- **Identity resolution:** `--identity <name>` explicit → `$ST_AGENT`
  → legacy `$ST_IDENTITY` → legacy `$COORD_IDENTITY` → throwaway
  `anon-<rand6>` (with a one-line stderr notice pointing at
  `ST_AGENT` for persistence). Same fallback chain as `coord mcp` in
  0.8.1.
- **`.mcp.json` bootstrap:** delegates to `cmdInit` — idempotent
  merge, divergent-entry prompt-gate. Channel mode defaults to `on`
  for claude, `off` for codex.
- **Claude session-id dance:** mirrors the `pty-claude-launcher.sh`
  reference — pins a `.claude-session-id` UUID (if the file doesn't
  exist), one-shot `claude --print` to bootstrap the jsonl when it's
  missing (avoids the "session runs in-memory only" trap under
  detached pty), then `claude --resume <SID>` for the persistent
  run.
- **Codex sidecar:** when the harness is `codex` and `pty` is on
  `$PATH`, the generated `pty.toml` includes a
  `[sessions.ding]` block running `coord ding <session> --identity
  <agent>` with `strategy = "permanent"` so it comes back after
  crashes — codex has no `asyncRewake` equivalent, so `coord ding`
  is the re-wake mechanism.
- **GLM path:** `--model <spec>` routes through `ollama launch
  <harness> --model <spec>` so ollama does the env injection AND
  skips its interactive model picker. Unblocks unattended
  GLM-backed agents.
- **pty-optional:** if `pty` is on `$PATH`, writes a minimal
  `pty.toml` (skip-if-exists — user edits are preserved) and hands
  off to `pty up`. If not, the dry-run prints the exact `pty.toml`
  snippet + direct-spawn command the user can drop in later.
- **`--dry-run`** (alias `--print`): print the identity /
  argv / mcp.json path / pty.toml preview / channel mode / ollama
  route summary without spawning anything. Also touches nothing on
  disk under dry-run.
- **New file:** `src/commands/launch.ts` + 32 unit tests covering
  identity resolution, channel-mode defaults, argv construction,
  pty.toml content, pty detection, session-id preservation, dry-run
  summary, and error paths.
- **Docs:** new README section "Bring a Codex (or Claude / GLM)
  agent onto smalltalk" — copy-pasteable, positioned right after
  `First time on a machine` so new readers hit it early.
- **VERSION** bumps to `0.9.0`.

Scope excluded per brief-016: no changes to pty's launcher itself.
The `.mcp.json` writer's actual bin-path resolution and the ollama
CLI shape are treated as external contracts.

### Fixed (`coord mcp` startup — anon-identity fallback)

`coord mcp` (and `st mcp` / `smalltalk mcp`) no longer hard-exits when
no `ST_AGENT` / `ST_IDENTITY` / `COORD_IDENTITY` is set. Instead the
server falls back to a throwaway `anon-<rand6>` agent (e.g.
`anon-h4k2qm`) and emits a single stderr warning that names the
throwaway id and points at `ST_AGENT` for persistence. The anon
agent's `inbox/` + `archive/` folders are lazy-created so the channel
watcher and status writer have something to point at.

This unblocks MCP hosts that spawn `coord mcp` without identity env
(Codex hit "cannot start the mcp server" before this fix). Managed
hosts that set an identity explicitly are unaffected — they keep
their explicit id and see no warning.

- Scope is `mcp` only. Other CLI verbs (`coord status`, `coord
  message send`, etc.) still require an explicit identity because
  their behavior is address-sensitive (silently sending FROM a
  fresh random id each invocation would mask user errors).
- The `anon-` prefix is stable — `st agents` listings and operators
  can spot throwaway sessions at a glance.
- The fallback honors the existing three-level chain: `ST_AGENT` →
  `ST_IDENTITY` (deprecation notice) → `COORD_IDENTITY` (deprecation
  notice) → `anon-<rand6>` (this new fallback).
- **VERSION** bumps to `0.8.1`.

### Added (brief-009 item 4 — SDK parity gap-fills)

The TS SDK already had near-complete parity with the CLI post-brief-009.
This entry closes the last four gaps surfaced by the audit. No CLI or
MCP surface change.

- **`coord.archive(id, fn, opts?: ArchiveOptions)`** — now takes an
  opts bag. `opts.withAttachments: true` mirrors the CLI's
  `--with-attachments` and moves prefix-sibling files alongside the
  canonical `.md`. Default unchanged (canonical `.md` only).
- **`coord.archiveTrim(id, opts)`** — `opts.withAttachments?: boolean`
  added to `TrimOptions`. When true, prefix-siblings of trimmed `.md`
  victims are also deleted from archive. Default unchanged.
- **`coord.lsOrphans(id?, opts?: { archive?: boolean })`** — new method.
  Returns `OrphanItem[]` (`{filename, ts}[]`) for prefix-sibling files
  whose canonical `.md` is no longer in the same folder. Mirrors
  `coord message ls --orphans`. Separate method (not an opt on `ls`)
  because the return shape differs — orphans have no frontmatter.
- **`coord.ding(deps)` on the handle** — thin wrapper around the
  already-exported `runDing`. `deps.identity` defaults to the Coord's
  own; `coord` is wired automatically. Useful for TUI / supervisor
  embedders that want to start a ding inside their own process
  instead of shelling out.
- **New exports from `@myobie/coord`:** `ArchiveOptions`, `OrphanItem`.
- **VERSION** bumps to `0.8.0`.

### Renamed (brief-009 item 3 — identity → agent)

**Soft-breaking with a deprecation chain.** The project's primary
noun changed from `identity` to `agent`. Every old name is kept as a
deprecated alias for one release cycle, so existing embedders /
running agents / consuming pty.toml configs all keep working
unchanged. Cos coordinates the per-machine pty.toml sweep over the
~8 downstream repos at her own pace.

- **SDK types:** `Agent` brand (replaces `Identity`); `asAgent` /
  `isAgent` (replace `asIdentity` / `isIdentity`). Old names are
  `@deprecated` re-exports pointing at the new brand — values are
  interchangeable.
- **SDK errors:** `AgentRequiredError` / `AgentNotHostedError` /
  `InvalidAgentError`. Old `Identity*Error` names are `@deprecated`
  consts aliased to the new classes — `instanceof` works either way.
  Error CODE strings (`IDENTITY_REQUIRED`, `IDENTITY_NOT_HOSTED`,
  `INVALID_IDENTITY`) stay stable as wire format. Error MESSAGE
  text changed ("identity required" → "agent required", etc.).
- **CLI verb:** `coord agents` (canonical) + `coord members`
  (deprecated alias) — both dispatch to the same handler.
- **MCP tool:** `coord_agents` + `st_agents` registered as the
  canonical names; `coord_members` + `st_members` kept as deprecated
  aliases pointing at the same handler. All four tool names work.
- **Env vars (cos coordinates):** `ST_AGENT` (preferred) → `ST_IDENTITY`
  (deprecated, warns once per process) → `COORD_IDENTITY` (legacy,
  warns once per process). The `[smalltalk] honoring … — migrate to
  ST_AGENT when convenient` notice fires per legacy hit. Per-machine
  `pty.toml` env blocks should migrate from `COORD_IDENTITY` /
  `ST_IDENTITY` to `ST_AGENT` at cos's pace; no flag day required.
- **SDK helpers:** `resolveAgent` / `envAgentFrom` (replace
  `resolveIdentity` / `envIdentityFrom`). Old names aliased.
- **Internal:** `validAgent` (replaces `validIdentity`); `cmdAgents`
  / `cmdAgentsCli` / `getAgents` / `listAgents` (replace `cmdMembers`
  / `cmdMembersCli` / `getMembers` / `listIdentities`). All old
  names aliased.
- **RESERVED_NAMES:** adds `agents`; keeps `members` (deprecated CLI
  verb name).
- **Field names on returned shapes** (e.g.
  `MessageWithLocation.identity`, `Overview.members`) — KEPT as-is
  for one release for back-compat with embedder destructures. A
  follow-up release will rename them to `.agent` / `.agents`.
- **`<channel source="coord" from="…">`** — KEPT as-is. Phase 5 of
  brief-005 (the `coord_*` tool-name drop) owns flipping this to
  `source="st"`.
- **VERSION** bumps to `0.7.0`.
- **Docs:** README, LAYOUT.md updated to lead with "agent" and the
  three-level env-var fallback.

Downstream sweep (cos owns): `[sessions.*.env].COORD_IDENTITY` (or
`ST_IDENTITY`) → `ST_AGENT` across ~8 pty.toml repos; agent boot
rituals referencing `coord_members` / `coord members` →
`coord_agents` / `coord agents`. Three-level fallback means nothing
breaks mid-sweep.

### Added (brief-009 item 5 — `resources/` surface)

A third optional per-identity folder for publishing annotated URLs to
peers. Each resource is `<unix-ms>-<rand6>.md` with `url:` in
frontmatter (required) and optional `title:` / `tags:` / `relation:`
/ body description. Mirrors the inbox-vs-archive single-writer rule:
`resources/` is owned by its identity; peers read via sync.

- **CLI:** `coord resource add <url> [--title T] [--tag T,T]
  [--relation REL] [--body-stdin]`, `coord resource ls [<identity>]
  [--json]`, `coord resource read [<identity>] <filename> [--json]`,
  `coord resource rm <filename>`.
- **SDK:** `coord.resources.{add,list,read,remove}` on the Coord
  handle. New types `Resource` + `ResourceWithLocation` re-exported
  from `@myobie/coord`.
- **MCP:** four new tools, dual-prefixed (`coord_resource_*` +
  `st_resource_*`) — `resource_add`, `resource_ls`, `resource_read`,
  `resource_remove`. Available in both channel and non-channel modes.
- **LAYOUT.md** documents the new folder + frontmatter shape.
- **RESERVED_NAMES** adds `resources` so an identity can't shadow the
  folder name.
- **New errors:** `ResourceNotFoundError`, `InvalidResourceUrlError`.
- **VERSION** bumps to `0.6.0`.

URL validation is intentionally lenient: any string with a scheme
prefix (`https://`, `pty://`, anything else an agent invents) is
accepted. The `pty://<session-name>` convention is documented but
not enforced.

The `relation:` field is **very optional** — absent by default,
**never inferred** from the URL / title / tags. The bare URL stays
first-class with or without it. Canonical (non-enforced) values:
`owns`, `relates-to`, `depends-on`. Agents may invent their own
relation strings; the schema is free-form.

### Docs (brief-009 add-on — onboard-a-friend support)

Three new notes added, plus a small update to an existing one, to
bring narrative docs in line with the slimmed-down surface and to
name the actor-model framing the system has always implicitly
assumed:

- **`notes/actor-model.md`** *(new)*: maps actor-model concepts —
  actor / mailbox / state / encapsulation / asynchrony — to coord's
  data shape. Provides the framing that makes the encapsulation rule
  ("across identities, only `inbox/` is writable") and the
  Coord-threads-stay-on-coord rule fall out as obvious consequences
  rather than ad-hoc conventions.
- **`notes/onboarding.md`** *(new)*: public zero-to-first-message
  recipe for a fresh participant (human or agent). Covers install,
  identity pick, status, send/receive, MCP wiring, and sync. The
  pre-existing `notes/agent-onboarding.md` is `.gitignore`'d (it's a
  myobie-specific machine runbook); this is the shippable
  counterpart.
- **`notes/repo-ownership.md`** *(new)*: codifies the
  `<repo>-claude` identity-naming convention and notes where the
  binding actually lives at runtime (`pty.toml`, `.mcp.json`). Points
  to brief-009 item 5 (resources) as the formal mechanism that will
  supersede the convention.
- **`notes/agent-roles.md`** *(minor update)*: reframed the future
  "external task tracker" paragraph to acknowledge tasks/journal are
  gone and point at the actor-model doc.

### Removed (brief-009 item 2 — `journal/` surface gone)

**Breaking.** The `journal/` folder and every CLI/MCP surface that
referenced it is removed. Same motivation as the tasks removal: paring
the surface to what the friend onboarding actually needs.

- **CLI:** `coord journal new/ls/cat/tail` deleted (\`src/commands/journal.ts\`
  removed).
- **MCP onboarding text:** the channel-mode instructions no longer
  reference journal entries; the boot ritual is now status + inbox-drain
  + members only.
- **MCP tidy-check:** the journal-lag drift condition is gone.
  Detection is **inbox staleness only**; \`DriftResult\` and
  \`DriftDetail\` shrank accordingly.
- **\`coord ding\`:** the tidy-line is now \`coord tidy-check: inbox=N
  (oldest Xm).\` (no journal segment).
- **RESERVED_NAMES:** \`journal\` is dropped.
- **Removed constant:** \`STALE_JOURNAL_MS\`.
- **Removed helper:** \`journalDir()\`.
- **Downstream impact:** consuming agents that reference \`coord
  journal\` in their boot rituals need to drop those steps. The cos
  agent owns sweeping the consuming agent CLAUDE.md files alongside
  the tasks-removal sweep.

### Removed (brief-009 item 1 — `tasks/` surface gone)

**Breaking.** The `tasks/` folder and every CLI/SDK/MCP surface that
referenced it is removed. Tasks were never widely used outside
myobie's own agents; the slim-down clears the way for a tighter
onboarding story.

- **CLI:** `coord task ...` and `coord tasks` subcommands deleted.
- **MCP onboarding text:** the channel-mode instructions no longer
  reference task-file ritual.
- **MCP tidy-check:** the `doingTask` drift condition is gone; the
  detector now covers inbox + journal-lag (journal-lag is removed in
  the next entry, item 2).
- **SDK:** no task types/methods were exposed (none existed); the
  `MemberTaskCounts` type and the `tasks` field on
  `MemberSummaryEnriched` / `coord_members` (enriched) are removed.
- **Public types:** `TaskState`, `TaskNotFoundError`,
  `TasksSingleWriterError`, `InvalidTaskTitleError`, and
  `InvalidTaskStateError` are no longer exported.
- **RESERVED_NAMES:** `tasks` is dropped (the name is once again
  available as an identity, though we'd advise against it).
- **Docs:** README, LAYOUT.md, completions guidance updated.
- **Downstream impact:** consuming agents that reference `coord task`
  / `coord tasks` in their boot rituals need to drop those steps. The
  cos agent owns sweeping the consuming agent CLAUDE.md files.

### Added (alias groundwork for the coord → smalltalk/st rename — Phase 0)

The package is being renamed to `smalltalk` (long) / `st` (canonical
short). This release lays down the **alias infrastructure** for that
rename. **Nothing breaks for callers in this release** — the legacy
`coord` surface continues to work end-to-end. Subsequent phases
(directory move, repo rename, per-agent config migration, cleanup) are
tracked separately.

- **Binary aliases.** Three commands install simultaneously: `st`
  (canonical), `smalltalk` (long form), `coord` (legacy alias). All
  three resolve to the same logic; `bin/coord` and `bin/smalltalk`
  resolve to `bin/st` via shell exec / symlink.
- **MCP server name dual-registration.** The server announces itself
  as `coord` when invoked through `bin/coord`, as `st` otherwise
  (`bin/st` and `bin/smalltalk`). Detection is via the bash shim
  capturing `$0` basename before any symlink walk, exported as
  `_ST_INVOKED_AS`.
- **MCP tool name dual-registration.** Every `coord_<verb>` tool —
  `coord_msg_send`, `coord_msg_ls`, `coord_msg_read`,
  `coord_msg_archive`, `coord_msg_thread`, `coord_msg_reply`,
  `coord_members` — is now ALSO registered as `st_<verb>` with the
  same schema and handler. Tools listings show 12 (or 14 in channel
  mode) entries instead of 6 (or 7).
- **Environment variable dual-honor.** `ST_IDENTITY` is preferred over
  `COORD_IDENTITY`; same for `ST_ROOT` over `COORD_ROOT`. When the
  legacy name is honored, a one-time-per-process stderr notice
  flags it: `[smalltalk] honoring COORD_IDENTITY — migrate to
  ST_IDENTITY when convenient`.
- **State directory resolution.** Default state path prefers
  `~/.local/state/smalltalk` when it exists, falls back to
  `~/.local/state/coord` when only that exists, and creates
  `~/.local/state/smalltalk` for brand-new installs. When both
  exist, `smalltalk/` wins silently (the env-var notice is the
  actionable signal). `ST_ROOT` / `COORD_ROOT` bypass this entirely.
- **Plugin proxy (git-style PATH dispatch).** Unknown subcommands
  fall back to a PATH lookup: `st-<cmd>` → `smalltalk-<cmd>` →
  `coord-<cmd>`. First executable match wins; built-in commands
  always take precedence over plugins of the same name. No `st-*`
  plugins ship with this release — the mechanism is greenfield,
  future-proofing.

### Hook script fixes (back-compat support)

`examples/codex/{session-start,stop}.sh` now capture stdout and stderr
separately when invoking `coord message ls --json`, so the new
`[smalltalk] honoring COORD_*` notice doesn't corrupt the captured
JSON payload. The failure-diagnostic path still surfaces the stderr
contents.

### Unchanged (Phase 0 deliberately preserves)

- `<channel source="coord" from="…">` notification frames keep
  `source="coord"`. Downstream parsers that grep this attribute
  continue to work unchanged. Phase 5 (cleanup) flips this to
  `source="st"` alongside the `coord_*` tool-name drop.
- Existing `.mcp.json` files pointing at `bin/coord` keep working.
- Existing scripts setting only `COORD_IDENTITY` / `COORD_ROOT` keep
  working (with the one-time migration notice).
- Existing `~/.local/state/coord/` directories keep working.

### Coming in later phases

- Phase 1: `~/.local/state/coord` → `~/.local/state/smalltalk`
  directory move (operational, cos-driven).
- Phase 2: GitHub repository rename + working-tree directory rename.
- Phase 3: per-identity rename, including `coord-claude` →
  `smalltalk-claude`.
- Phase 4: per-agent `.mcp.json` / `settings.local.json` / `pty.toml`
  migrations to point at `bin/st` and use `ST_*` env vars.
- Phase 5: drop the `coord_*` tool aliases, the `COORD_*` env
  fallbacks, and the `bin/coord` shim. Flip channel `source` to
  `"st"`. Bump major version.
