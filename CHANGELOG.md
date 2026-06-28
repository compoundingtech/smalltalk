# Changelog

All notable changes to `@myobie/coord` (renaming → `@myobie/smalltalk`) are
recorded here. The project is pre-1.0; expect breaking changes in
minor releases until 1.0.

## Unreleased

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
