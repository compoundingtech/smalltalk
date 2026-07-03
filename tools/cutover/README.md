# tools/cutover — coord → st sweep scripts

Rewriters for the per-machine cutover step. Two shapes covered:

- **`.mcp.json`** — every agent's Claude Code MCP config that
  currently references `coord` (as a `mcpServers` key, as a `bin/coord`
  path, or via `COORD_*` env vars).
- **`pty.toml`** — every pty session config with a `server:coord`
  channel token, a `coord ding` sidecar, or `COORD_*` env vars.

## Design

Each rewriter is a **pure function** on the file text:

```ts
rewriteMcpJson(text: string): { text, changed, actions }
rewritePtyToml(text: string): { text, changed, actions }
```

The driver (`sweep.ts`) walks a directory tree and applies both.
Idempotent: running twice against the same tree is a no-op the
second time. Backups (`<name>.pre-cutover`) are written on first
change; skip-if-exists on the backup so re-runs don't clobber the
original snapshot.

## Rules

### `.mcp.json`

1. `mcpServers.coord` → `mcpServers.st` (rename the JSON key). If
   both `coord` and `st` entries exist, drop `coord` and keep `st`.
2. `command` path rewrite. Any of these suffixes get flipped to
   `myobie/smalltalk/bin/st`:
   - `myobie/coord/bin/coord`  (old repo, old bin)
   - `myobie/smalltalk/bin/coord`  (new repo, old bin)
   - `myobie/coord/bin/st`  (old repo, new bin)
   - `myobie/coord/bin/smalltalk`  (old repo, alias bin)
3. `env.COORD_IDENTITY` → `env.ST_AGENT`. If both are present, drop
   the legacy. Same rule for the `ST_IDENTITY` legacy alias.
4. `env.COORD_ROOT` → `env.ST_ROOT` (same conflict rule).
5. `env.COORD_CONFIG` → `env.ST_CONFIG` (same conflict rule).

### `pty.toml`

Line-based regex sweep (no full TOML round-trip — preserves
formatting, comments, blank lines):

1. `command = "... server:coord ..."` → `... server:st ...`
   (with a negative-lookahead guard so `server:coord-web` etc.
   don't false-match).
2. `command = "... coord ding <arg>"` → `... st ding <arg>`
   (word-boundary guard: `coord dingus` etc. don't match).
3. Inside `[sessions.<name>.env]` blocks only:
   - `COORD_IDENTITY = "..."` → `ST_AGENT = "..."`
     (drop when `ST_AGENT` is already set in the same block).
   - `ST_IDENTITY = "..."` → drop when `ST_AGENT` set.
   - `COORD_ROOT = "..."` → `ST_ROOT = "..."` (same conflict rule).
   - `COORD_CONFIG = "..."` → `ST_CONFIG = "..."` (same).

## Usage

```sh
# Dry-run first — audit what would change.
node --experimental-strip-types tools/cutover/sweep.ts \
    /Volumes/SSD/src/github.com/myobie \
    ~/.dot-files \
    --dry-run

# Then commit.
node --experimental-strip-types tools/cutover/sweep.ts \
    /Volumes/SSD/src/github.com/myobie \
    ~/.dot-files
```

Flags:

- `--dry-run` — print planned actions; touch nothing on disk.
- `--kind mcp-json|pty-toml|both` — restrict to one file type.
  Default `both`.
- `--depth <n>` — recursion depth per root. Default `4`.
- `--no-backup` — skip writing `<name>.pre-cutover` alongside each
  changed file. On by default.

Exit codes: `0` clean sweep, `1` at least one file couldn't be
parsed and was skipped, `2` bad CLI args.

## Tests

Pure-function tests live at
`tests/unit/tools/cutover/rewrite-{mcp-json,pty-toml}.test.ts` and
run under the standard `npx vitest run` suite. 31 cases cover:

- Every rewrite rule above.
- Idempotence (running against post-cutover input is a no-op).
- Backup / conflict edge cases (both `coord` and `st` entries
  present, ST_AGENT + COORD_IDENTITY both set, etc.).
- Word-boundary guards (`server:coord-web`, `coord dingus`).
- Malformed input (unparseable JSON throws; caller-decides
  whether to skip).
- Multi-session pty.toml (each `[sessions.<name>.env]` block scoped
  independently).

## Not in scope

- `~/.local/state/coord/` migration — that's a plain `rsync -a
  ~/.local/state/coord/ ~/.local/state/smalltalk/` before the code
  merges land. No file-content rewriting.
- `~/.dot-files/ai/plugins/coord/` plugin rename — a directory
  move + a hand edit of `SKILL.md`, not a mechanical sweep. cos owns
  the plugin edit.
- CHANGELOG / docs `coord` references — surgical prose edits, not a
  mechanical sweep. Done in a separate PR.
