# Changelog

All notable changes to `@myobie/coord` (renaming → `@myobie/smalltalk`) are
recorded here. The project is pre-1.0; expect breaking changes in
minor releases until 1.0.

## Unreleased

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
