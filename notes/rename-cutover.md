# brief — coord → st cutover (weekend, zero-coord by Nathan's demo)

**Owners.** Code map: smalltalk-claude. Agent restart choreography: cos.
Landing: Nathan merges.

**Goal.** After this cutover, no visible surface says "coord" — no MCP
tool prefix, no env var, no CLI binary, no `.mcp.json` entry key, no
state dir. Comments and historical brief notes may still say "coord"
where they refer to the pre-rename era.

**Scope numbers** (measured on `main` @ `9a2a462`, 2026-07-03):

| Surface                    | Files | Notes                                        |
|----------------------------|-------|----------------------------------------------|
| `src/**` code refs         | 39    | 5719 raw `coord`-substring hits              |
| `tests/**` refs            | 65    | Most are `COORD_*` env in fixtures           |
| `examples/**` refs         | 13    | Includes `.mcp.json` example + hook scripts  |
| `notes/**` refs            | 37    | Historical briefs — surgical, not mass-edit  |
| `bin/**`                   | 2     | `bin/coord` shim + `bin/smalltalk` symlink   |
| Root docs (`*.md`, JSON)   | 7     | `CHANGELOG.md`, `LAYOUT.md`, `README.md`, etc |

Plus external, per-machine state on `myobie's` box:

| Surface                                              | Count observed |
|------------------------------------------------------|----------------|
| `~/.local/state/coord/<agent>/` dirs                 | 42 agent trees |
| `~/.dot-files/ai/plugins/coord/skills/coord/SKILL.md`| 1 (cos owns)   |
| `.mcp.json` in Nathan's repos                        | ~10 sampled    |
| `pty.toml` files with `COORD_*` or `coord ` refs     | ≥10 sampled    |

The `.mcp.json` files fall into TWO shapes we need to sweep:

- Point at `github.com/myobie/coord/bin/coord` — **the OLD repo**, still
  live for some agents (reminders, cos, planecast, etc.). Those repos
  don't exist on this branch; the shim resolves against the older
  smalltalk clone.
- Point at `github.com/myobie/smalltalk/bin/coord` — the NEW repo but
  the OLD binary name (this repo's own `.mcp.json`, evals, transplant,
  etc.).

Both need to end up at `github.com/myobie/smalltalk/bin/st` after the
cutover, wrapped under the JSON key `st` (not `coord`).

---

## Rename surface — inventory + target

### A. MCP wire surface (highest impact — every agent breaks if wrong)

| Item                              | Current                                               | Target                          | Where              |
|-----------------------------------|-------------------------------------------------------|---------------------------------|--------------------|
| MCP tool names                    | Dual: `coord_*` + `st_*` (14 base names)              | `st_*` only                     | `src/mcp/tools/dual-register.ts`, all tool registrations |
| MCP server `name` field           | `'coord'` when invoked as `coord`, `'st'` otherwise    | Always `'st'`                   | `src/mcp/capabilities.ts` (`SERVER_INFO`, `buildServerInfo`) |
| Channel token `source=…`          | `<channel source="coord" …>`                          | `<channel source="st" …>`       | `src/mcp/channel-watcher.ts` — actually driven by CHANNEL_INSTRUCTIONS string, not emitted from channel-watcher directly |
| Claude Code channel-load flag     | `--dangerously-load-development-channels server:coord` OR `server:st`  | `server:st`  | `src/commands/launch.ts` already emits `server:st`; verify no `server:coord` fallback |
| `CHANNEL_INSTRUCTIONS` string     | Uses `coord_msg_*`, `coord status`, `source="coord"`  | `st_msg_*`, `st status`, `source="st"` | `src/mcp/capabilities.ts:97-110` (11-line block) |
| `EXPECTED_TOOL_NAMES` const       | Emits both `coord_*` + `st_*`                         | `st_*` only                     | `src/mcp/capabilities.ts:138-168` |
| `mcp/tools/dual-register.ts`      | Registers both prefixes                               | Delete file OR rename to `single-register.ts` registering `st_*` only | Same file |

The **`registerDualTool` → `registerStTool`** switch is the single
biggest wire-surface change. After it lands, an agent still holding the
old `coord_*` names in-context will get "unknown tool" errors on its
next call. Agents must be cold-restarted onto the new
`CHANNEL_INSTRUCTIONS` before they lose `coord_*` availability. This is
why the tool-drop sits at the END of the code-side sequence, AFTER
every agent has been restarted onto the new instructions.

### B. Environment variables

| Var                          | Current chain                                        | Target                  |
|------------------------------|------------------------------------------------------|-------------------------|
| Agent identity               | `ST_AGENT` → `ST_IDENTITY` → `COORD_IDENTITY` (both fallbacks warn once) | `ST_AGENT` only |
| Root path                    | `ST_ROOT` → `COORD_ROOT` (warn once)                 | `ST_ROOT` only          |
| Config dir                   | `COORD_CONFIG` (no `ST_CONFIG` yet)                   | `ST_CONFIG`, `COORD_CONFIG` fallback with warn — then drop |
| Channel debug                | `COORD_CHANNEL_DEBUG=1`                              | `ST_CHANNEL_DEBUG=1`    |
| Error meta key               | `COORD_ERROR_META_KEY`                               | `ST_ERROR_META_KEY`     |
| Hook freshness threshold     | `COORD_PRECOMPACT_FRESH_S`                           | `ST_PRECOMPACT_FRESH_S` |
| Hook timeout                 | `COORD_PRECOMPACT_TIMEOUT_S`                         | `ST_PRECOMPACT_TIMEOUT_S` |
| Hook rehydrate staleness     | `COORD_REHYDRATE_STALE_S`                            | `ST_REHYDRATE_STALE_S`  |
| Claude permission-mode       | `CLAUDE_PERMISSION_MODE` (no `coord` in name)         | unchanged               |
| Invoked-as internal marker   | `_ST_INVOKED_AS`                                     | unchanged (already ST_) |

**Add ST_CONFIG now** (currently missing) so the cutover can drop
COORD_CONFIG without a gap. Small precursor PR.

### C. Filesystem

| Path                                | Current                                       | Target                                  |
|-------------------------------------|-----------------------------------------------|-----------------------------------------|
| State root                          | `~/.local/state/coord/` OR `~/.local/state/smalltalk/` (code prefers `smalltalk/` when both exist) | `~/.local/state/smalltalk/` |
| Config dir                          | `~/.config/coord/` (per `coordConfigFrom`)    | `~/.config/smalltalk/`                  |
| Fish completions                    | `~/.config/fish/completions/coord.fish`       | `~/.config/fish/completions/st.fish`    |
| Package name                        | `@myobie/coord`                               | `@myobie/smalltalk`                     |
| Repo dir                            | `github.com/myobie/smalltalk/` (already right; older `.mcp.json` still points at `github.com/myobie/coord/`) | Every `.mcp.json` updated to `smalltalk/` path |

### D. CLI

| Item                     | Current                                | Target                       |
|--------------------------|----------------------------------------|------------------------------|
| Primary binary           | `bin/st`                               | unchanged                    |
| Alias binary             | `bin/smalltalk` → `st` symlink         | unchanged                    |
| Back-compat binary       | `bin/coord` (bash shim exec'ing `st`)  | **Delete in Phase D** — no rename shim left after cutover |
| `package.json.bin.coord` | maps to `./bin/coord`                  | **Delete**                   |
| `USAGE`, `HELP` strings  | say "coord <verb>" throughout          | "st <verb>"                  |
| `resolveCoordBinPath`    | walks package.json for `@myobie/coord`, falls back to `which coord` | Rename to `resolveStBinPath`; walk for `@myobie/smalltalk`; fall back to `which st` |
| Fallback binary lookup   | `spawnSync('which', ['coord'])`        | `spawnSync('which', ['st'])` |

### E. TypeScript identifiers (safe, mechanical)

| Symbol / file                                | Current                     | Target                    |
|----------------------------------------------|-----------------------------|---------------------------|
| Type                                         | `Coord`                     | `St` (short; matches CLI) |
| Factory                                      | `createCoord`               | `createSt`                |
| Options type                                 | `CoordOptions`              | `StOptions`               |
| Base error class                             | `CoordError`                | `StError` (JSDoc alias `CoordError` for one release) |
| `coordRoot()`, `coordRootFrom()`             | fn names                    | `stRoot`, `stRootFrom`    |
| `coordConfig()`, `coordConfigFrom()`         | fn names                    | `stConfig`, `stConfigFrom` |
| `warnCoordFallback`                          | private helper              | `warnStFallback`          |
| `invokedAsFrom(env)` → `'coord' \| 'st'`     | union                       | `'st'` only               |
| `canonicalServerName(invoked)`               | maps `smalltalk`→`st`, `coord`→`coord` | Remove; server always `'st'` |
| Test file `lib-coord-api.test.ts`            | filename                    | `lib-st-api.test.ts`      |

Error CODE strings (`IDENTITY_NOT_HOSTED`, `IDENTITY_REQUIRED`, etc.)
stay stable — they're pattern-matched by embedders. Class names change,
codes don't. There's already a rename precedent: `Identity*Error`
class → `Agent*Error` with codes kept as `IDENTITY_*`.

### F. Docs (find-replace + review)

| File                     | Approach                                                                        |
|--------------------------|---------------------------------------------------------------------------------|
| `CHANGELOG.md`           | Header + description mentions `@myobie/coord` — flip to smalltalk. Historical entries retain their language ("brief-020 fixed the coord channel-watcher…") |
| `LAYOUT.md`              | s/coord/smalltalk/ throughout — LAYOUT-004 grammar is unaffected                |
| `README.md`              | Full rewrite of the intro; examples flip                                        |
| `IDEA.md`, `CONTRIBUTING.md` | Read + edit                                                                   |
| `notes/agent-onboarding.md` | Update — this is what new agents read                                          |
| `notes/agent-roles.md`   | Update                                                                          |
| `notes/PROOF.md`, `walkthrough.md`, `v0-walkthrough.md` | Update user-facing snippets. Historical framing OK.        |
| `notes/brief-005-*.md` (renaming intro) | Leave — historical brief that predates cutover.                        |
| Other `notes/brief-*.md` | Leave unless a code snippet reads misleadingly post-cutover.                    |
| `examples/claude-code/README.md` | Update — this is user-facing.                                          |
| `examples/claude-code/settings.local.example.json` | Update `_comment` + hook paths                          |
| `examples/claude-code/hooks/*.sh` | Update comments referencing "coord PreCompact hook", `<coord-root>`      |

### G. Tests

Two flavors:

- **Environment fixtures** using `COORD_IDENTITY` / `COORD_ROOT` etc.
  Sweep to `ST_AGENT` / `ST_ROOT`. Currently the fallback tests
  specifically exercise the deprecation path — keep those, but the
  default fixtures shouldn't set `COORD_*`.
- **Instructions regression** (`tests/unit/channel-instructions.test.ts`)
  which asserts load-bearing substrings in `CHANNEL_INSTRUCTIONS`. Update
  the substring list to `st_msg_*` / `st status` / `source="st"`.
- The **deprecation-alias tests** (`tests/unit/aliases.test.ts`) stay
  around until the fallback code is deleted, then they get deleted with
  the code.

### H. External / per-machine

Nothing in this repo. **cos owns coordination:**

- `~/.dot-files/ai/plugins/coord/skills/coord/SKILL.md` — rename plugin
  dir + rewrite skill body from `coord_msg_*` / `$COORD_ROOT` to
  `st_msg_*` / `$ST_ROOT`.
- Every `.mcp.json` in every agent repo — flip
  `{ "coord": {"command": ".../bin/coord", … } }` to
  `{ "st": {"command": ".../smalltalk/bin/st", … } }`. Wrong-repo paths
  (`myobie/coord/bin/coord`) get corrected to
  `myobie/smalltalk/bin/st` in the same edit.
- Every `pty.toml` — `COORD_IDENTITY=…` → `ST_AGENT=…`; `coord ding …`
  → `st ding …`; `server:coord` → `server:st`.
- `~/.local/state/coord/` → `~/.local/state/smalltalk/` per-machine
  migration (rsync `-a`, keep old for 24-72h rollback window then rm).

---

## Safe order (dependency-driven)

Anything that changes the **wire the running agents rely on** must
land AFTER agents have been restarted onto instructions that use the
new wire. Anything that changes **only local state on disk** can
happen anytime as long as the fallback still reads it.

### Phase P — precursors (mergeable now, no cutover impact)

Nothing here changes running-agent behavior. Land these before cutover
weekend to shrink the weekend diff.

1. **P1. `ST_CONFIG` env var + `stConfigFrom` fallback.** Currently
   only `COORD_CONFIG` reads exist; add the ST-preferred var with the
   COORD_ fallback + one-time warn. Enables Phase D to drop
   `COORD_CONFIG` without a gap.

2. **P2. Rename `bin/coord` shim's internal comments** to say "back-compat
   alias" without behavior change. Signposts the cutover for anyone
   reading the shim between now and Phase C.

3. **P3. Land PR #14 (dual-honor aliases groundwork).** Already open.
   Re-verify it still applies against `main` (post-#33 merge). Merge
   or rebase-close before Phase C so the cutover diff isn't racing
   its own precursor.

4. **P4. Land PR #28 (brief-020 tests) and PR #29 (persona).** Both are
   substantive standalone additions unrelated to the rename; landing
   before cutover keeps Phase C's diff focused on the rename itself.
   Flag both to Nathan for merge decision — see "Old PR status" below.

5. **P5. Snapshot the current CHANNEL_INSTRUCTIONS.** cos publishes the
   plaintext of the NEW instructions to every agent's inbox via `st
   message send` — one copy per agent, subject "channel-instructions
   snapshot — cutover pre-read." Agents read it into their working
   memory. When cutover flips the instructions on restart, the new
   text isn't a surprise.

### Phase C — cutover weekend (coordinated pause + restart)

**Trigger.** cos pins a UTC time. All agents receive a "freeze at HH:MM
UTC — cutover starting" ping via `coord message send` 15 min before.

**Step by step:**

1. **C0. Freeze.** cos sets every agent's status to `busy` (or offline
   for eval agents) to signal to peers not to `msg_send` them. Nathan +
   cos are the only two writers during the window.

2. **C1. State-root migration.** On each host:
   ```
   rsync -a ~/.local/state/coord/ ~/.local/state/smalltalk/
   ```
   No `--delete`. The old tree stays as rollback. Verify identity dirs
   and status files copied. **~42 agents on this box.**

3. **C2. Code merges (in this order, single machine at a time):**

   a. **C2.1** Merge the "wire prep" PR:
      - `CHANNEL_INSTRUCTIONS` rewritten to use `st_*` / `st ` / `source="st"`.
      - `buildServerInfo` returns `name: 'st'` unconditionally.
      - `SERVER_INFO.name = 'st'` default.
      - `examples/claude-code/README.md` + `settings.local.example.json`
        + hook script comments updated.
      - PR #14's `EXPECTED_TOOL_NAMES` and dual-registration REMAIN as
        `coord_* + st_*` at this stage. This PR only changes what the
        SERVER says; the coord_* tools still resolve.

   b. **C2.2** Update every `.mcp.json` in Nathan's repos (script it —
      cos runs). Swap the JSON key `"coord"` → `"st"`; swap `command`
      path to `.../smalltalk/bin/st`; drop `env: {COORD_IDENTITY, COORD_ROOT}`.
      This is a mechanical sed job — see "cutover script sketch"
      below.

   c. **C2.3** Update every `pty.toml` (script). Swap
      `COORD_IDENTITY = "x"` → `ST_AGENT = "x"`; swap `coord ding …` →
      `st ding …`; swap `server:coord` → `server:st`.

   d. **C2.4** cos edits the plugin: rename
      `~/.dot-files/ai/plugins/coord/` → `~/.dot-files/ai/plugins/st/`;
      rewrite `SKILL.md` from `coord_*` / `$COORD_ROOT` to `st_*` /
      `$ST_ROOT`.

3. **C3. Cold-restart every agent** with fresh env: only `ST_AGENT` +
   `ST_ROOT` (+ `PATH` including `smalltalk/bin`) set. No `COORD_*` env
   vars in the launcher. Confirm each agent's boot ritual runs against
   the new instructions (they'll call `st_msg_ls` on wake).

4. **C4. Verify** for each agent:
   - `st agents` from myobie's shell shows every peer as `available`
   - Channel notifications include `source="st"`
   - `st_*` tools resolve (list one via any agent)
   - No `[smalltalk] honoring COORD_*` fallback warnings in
     stderr on cold-start

### Phase D — deprecation cleanup (post-cutover, weekday-safe)

After Phase C is verified stable for ≥ 24h:

1. **D1. Drop `coord_*` MCP dual-registration.** Delete
   `mcp/tools/dual-register.ts` OR rename to `single-register.ts`
   registering only `st_*`. Update `EXPECTED_TOOL_NAMES` to `st_*` only.
   Every tool file (`src/mcp/tools/*.ts`) drops its `coord_*` call.
   Update `capabilities.ts` comment blocks.

2. **D2. Drop `COORD_*` env var fallback.** Delete `warnCoordFallback`
   calls in `common.ts:envAgentFrom` and `coordRootFrom`. Delete
   `_resetCoordFallbackWarnings` test helper. Delete the
   deprecation-alias tests that exercised the fallback path.

3. **D3. Drop `bin/coord` shim.** Delete `bin/coord`. Drop
   `package.json.bin.coord`.

4. **D4. Rename TS identifiers.** `Coord` → `St`, `createCoord` →
   `createSt`, `CoordError` → `StError`, `coordRoot`/`coordConfig` →
   `stRoot`/`stConfig`, etc. Keep the old class names as deprecated
   aliases for ONE release (`export const Coord = St`) since embedders
   like the pi extension may still `import { Coord } from '@myobie/smalltalk'`.

5. **D5. Package rename.** `package.json.name` → `@myobie/smalltalk`.
   Description updated. `bin` map drops `coord`. Publish a final
   `0.9.x` under the old name that only re-exports from the new
   package, if we want the npm-install path to keep working. (Skip if
   nobody depends on npm resolution.)

6. **D6. Config dir + file rename.** Drop `~/.config/coord/` support
   (Phase P1 already added `~/.config/smalltalk/`). Update
   `~/.config/fish/completions/coord.fish` filename to `st.fish`.

7. **D7. Delete `~/.local/state/coord/` on each host.** Only after ≥ 72h
   of stable Phase C. `rm -rf`.

8. **D8. Historical notes.** Do a final read of `notes/*.md` and
   `examples/claude-code/README.md` for anything that would confuse a
   NEW reader (someone joining post-cutover) — surgical edits only.
   Don't rewrite briefs that describe the pre-rename era.

---

## Which agents restart onto clean env

Every agent on this box gets cold-restarted during Phase C3. cos owns
the choreography; the machine layout has ~42 agent trees under
`~/.local/state/coord/`.

Restart contract:

- Fresh env — only `ST_AGENT=<agent>` + `ST_ROOT=$HOME/.local/state/smalltalk`
  (+ `PATH` including `.../smalltalk/bin`). No `COORD_*` set.
- Fresh Claude Code / codex session (no `--resume` first — get a clean
  jsonl so the new `CHANNEL_INSTRUCTIONS` seed the context).
- Post-boot, the agent's first turn is the boot ritual against the new
  instructions: `st status <me> --set available`, then `st_msg_ls`, etc.

Watch-listed agents (cos + me discuss individually):

- **evals-claude, evals-worker** — live evals; may need scheduled
  windows.
- **pty-claude, pty-relay-claude** — supervise other agents; restart
  ORDER matters (restart the supervised agents first so pty-claude
  sees them coming back up).
- **pi-extension-*** — pi extension has an analogous `session_start`
  handler; verify it still fires after the rename.
- **glm-*, ollama-*** — model-backed; slow first turn (see
  evals-claude's GLM handoff message). May need extra boot-wait budget.
- **cos + smalltalk-claude** — us. Restart last so we can drive the
  cutover from a still-live session.

---

## Cutover script sketches (drafts)

### `.mcp.json` sweeper

Runs on Nathan's `~/`:

```sh
# ./tools/cutover-mcp-json.sh
set -euo pipefail
find /Volumes/SSD/src/github.com/myobie \
     ~/.dot-files \
     -maxdepth 3 -name ".mcp.json" -print0 | while IFS= read -r -d '' f; do
  cp "$f" "$f.pre-cutover"
  python3 - <<PY "$f"
import json, sys
p = sys.argv[1]
with open(p) as fp: data = json.load(fp)
srv = data.get("mcpServers", {})
if "coord" in srv:
    entry = srv.pop("coord")
    cmd = entry.get("command", "")
    entry["command"] = cmd.replace("myobie/coord/bin/coord", "myobie/smalltalk/bin/st") \
                          .replace("myobie/smalltalk/bin/coord", "myobie/smalltalk/bin/st")
    env = entry.get("env", {})
    if "COORD_IDENTITY" in env:
        env["ST_AGENT"] = env.pop("COORD_IDENTITY")
    if "COORD_ROOT" in env:
        env["ST_ROOT"] = env.pop("COORD_ROOT")
    srv["st"] = entry
    data["mcpServers"] = srv
with open(p, "w") as fp: json.dump(data, fp, indent=2); fp.write("\n")
PY
done
```

### `pty.toml` sweeper

Similar shape — sed swap of env keys + command tokens. Backups first.

### State-root rsync

```sh
rsync -a ~/.local/state/coord/ ~/.local/state/smalltalk/
```

No `--delete`. Old tree stays put for 72h.

---

## Risks + mitigation

- **Missed .mcp.json.** An agent whose config still reads the old key
  won't connect on restart. Mitigation: cos's sweep script + Phase C4
  verification step (`st agents` per peer).
- **Instruction seed lag.** An agent restarted BEFORE its
  `CHANNEL_INSTRUCTIONS` snapshot lands sees old text. Mitigation:
  Phase P5 — pre-cutover snapshot goes out ahead.
- **coord_* tool orphans.** An in-flight agent that hasn't restarted
  during Phase D1 will get "unknown tool" on `coord_msg_*` calls.
  Mitigation: Phase D1 waits until every agent has been through
  Phase C3. If any agent is still alive on `coord_*`, delay D1.
- **Older-repo path (`myobie/coord/bin/coord`).** ≥5 `.mcp.json` files
  point at the old repo dir. If the old repo dir still exists on
  disk, they'll keep working through Phase C and require the sweep
  script to redirect them. Mitigation: sweeper handles this case
  (see the `.replace("myobie/coord/bin/coord", …)` line).
- **`_ST_INVOKED_AS` reading `coord`.** After Phase D3 (deleting
  `bin/coord`), any stale process spawned via the old shim will get
  `_ST_INVOKED_AS=coord` and (post-D1) hit code that assumes `st`.
  Mitigation: `invokedAsFrom` returns `'st'` unconditionally post-D3.
- **pi extension out of sync.** The pi extension has its own
  `session_start` handler and may pattern-match server name /
  channel token. Mitigation: check `examples/pi/coord.ts` (yes, that's
  the filename) during Phase C2.1 review; cos coordinates the pi-side
  change.
- **npm-install path.** If anything external depends on
  `require('@myobie/coord')`, Phase D5 breaks it. Mitigation:
  publish a final legacy release that re-exports from the new
  package, OR confirm nothing external depends on npm resolution.

---

## Old PR status (flagged per cos's ask)

- **PR #14** (older `github.com/myobie/coord` repo — dual-honor alias
  groundwork). **Folds into this cutover.** The dual-honor logic is
  already effectively landed on `main` here via the ST_ / COORD_
  fallback chain in `common.ts`. Recommendation: **close #14 as
  superseded** and reference this brief in the close comment; the same
  outcome is achieved by Phase C + D in one motion.

- **PR #28** (brief-020 tests: live channel-wake green + idle-wake
  variant). Independent of the rename. Two infra fixes + a new idle
  test. **Recommendation: merge before cutover** — Phase P4. It
  provides an empirical checkpoint ("idle wake works") that we lose
  visibility on if we don't run it before flipping the wire.

- **PR #29** (brief-022 st launch `--persona`). Independent of the
  rename. Small, self-contained. **Recommendation: merge before
  cutover** — Phase P4. Later PRs (#35 asyncRewake) don't collide with
  the persona surface, and having `--persona` in the launcher matters
  for Nathan's demo prep.

Both #28 and #29 will need trivial rebases post-cutover if they land
after — safer to land before.

- **PR #34** (task #128 outside-md) — under Nathan review. Independent
  of the rename. No cutover interaction. Merge when Nathan's ready.

- **PR #35** (task #118 asyncRewake) — under Nathan review. Independent
  of the rename. No cutover interaction. Merge when Nathan's ready.

---

## Deliverable checklist (what "done" looks like)

- [ ] `grep -r coord src/ tests/ examples/ *.md` returns only historical
      brief references + package rename historical mentions in CHANGELOG.
- [ ] `~/.local/state/coord/` deleted on every host.
- [ ] `~/.config/coord/` deleted (or renamed to `smalltalk/`).
- [ ] Every `.mcp.json` on Nathan's machine has `"st"` as the sole
      `mcpServers` key, pointing at `.../smalltalk/bin/st`.
- [ ] Every `pty.toml` has `ST_AGENT` / `st ding` / `server:st`.
- [ ] Every agent restarted cleanly onto the new env at least once.
- [ ] `st agents` shows every peer as `available` and no `unknown`.
- [ ] `bin/coord` no longer exists in the repo.
- [ ] `package.json.name` = `@myobie/smalltalk`.
- [ ] `SERVER_INFO.name` = `'st'`, no `coord`/`st` conditional.
- [ ] MCP `tools/list` returns only `st_*` — no `coord_*` entries.
- [ ] `CHANNEL_INSTRUCTIONS` snapshot pinned for post-cutover reference.

Standing by for cos's review of this plan. We execute together; I own
the code, cos owns the agent choreography.
