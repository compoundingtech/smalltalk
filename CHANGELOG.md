# Changelog

All notable changes to `@myobie/coord` (renaming → `@myobie/smalltalk`) are
recorded here. The project is pre-1.0; expect breaking changes in
minor releases until 1.0.

## Unreleased

### Changed (coord-kill piece (d) — cosmetic scrub: README, comments, log strings, variable names)

Comprehensive scrub of remaining `coord`/`Coord` references across
source + tests + hook scripts + README.

- **`coord` variable names** renamed to `st` — the parameter
  `(coord: St)` → `(st: St)` across all MCP tool files, the local
  `const coord = createSt(...)` → `const st = createSt(...)` in
  `lib.ts` + `mcp/index.ts` + `ding.ts`, and every downstream
  `coord.method()` call → `st.method()`.
- **DingDeps + McpServerHandle interface fields** renamed
  `coord: St` → `st: St`. Consumers updated.
- **Test fixture `FakeCoord`** renamed to `FakeSt`; test helper
  `makeFakeCoord` → `makeFakeSt`. All `handle.coord` /
  `fake.coord` refs updated to `handle.st` / `fake.st`.
- **README** — the "Note on the name" dual-alias section reduced
  to a one-line note; the "Names" section rewritten to describe
  the post-cutover surface (`st` + `smalltalk` binaries, `st_*`
  MCP tools, `ST_AGENT`/`ST_ROOT`/`ST_CONFIG` env, no coord
  fallback). Programmatic-API example updated to
  `import { createSt, ... } from '@myobie/coord'` (npm scope
  preserved — it's the package identity, not a personal-brand
  ref). MCP server section rewritten to describe the `st_*`-only
  tool surface and `_meta['st/error']` wire key.
- **Command file header comments** — 17 files' header comments
  scrubbed of `coord` verb refs.
- **`coord-web`** in comments → `the web UI` (generic — external
  product isn't shipped by this repo).
- **`~/.local/state/st` / `~/.config/st`** stray path segments
  from an over-aggressive bulk sed corrected back to
  `~/.local/state/smalltalk` / `~/.config/smalltalk` (the state
  root name is `smalltalk`, not the CLI-command name `st`).
- **`createSt` default `configRoot`** — was `~/.config/st`,
  corrected to `~/.config/smalltalk`.
- **`parsePeer` default bare-hostname resolution** — was
  `<host>:.local/state/st/`, corrected to
  `<host>:.local/state/smalltalk/`.
- **`sync.ts` resolvePeer** — same correction for the peer
  fallback path.
- **`tidy-check` notification `from` field** — was
  `'coord-system'`, now `'st-system'`. Test updated.
- **`completions` output** — regenerated from generic scrub;
  test asserts `complete -c st` (not `-c coord`).
- **Wire meta key** — `_meta['coord/error']` → `_meta['st/error']`
  refs in error-mapping module and every consuming test.
- **`stErrorToToolResult`** function (was `coordErrorToToolResult`)
  and `readStErrorPayload` (was `readCoordErrorPayload`) — public
  error-mapping exports renamed.

Test fixture data cosmetic:
- `myobie` (agent name used as test fixture identity) renamed to
  `operator` in test-only fixtures. Load-bearing exception: NPM
  scope refs like `@myobie/coord` and `@myobie/pty` are preserved
  — they're the actual package identifiers.
- The 3-segment legacy filename regression test uses `legacy` as
  the middle segment (was `myobie`) — semantic behavior of the
  test is unchanged.

Vestigial migration tools removed:
- `tools/cutover/rewrite-mcp-json.ts` + tests
- `tools/cutover/rewrite-pty-toml.ts` + tests
- `tools/cutover/sweep.ts` + tests
- `tools/cutover/` directory itself

Full suite: 1555 pass, 3 pre-existing integration skipped.
Pre-push name-hygiene grep: clean (with `@myobie/` npm scope
excluded per the updated memory'd pattern).

Held on `feat/kill-coord-entirely` until the reboot signal.

### Changed (coord-kill piece (c) — SDK / wire renames: `CoordError` → `StError`, `createCoord` → `createSt`, `Coord` → `St`, wire meta key flip)

Public SDK surface and wire-format constants renamed to their
post-cutover canonical shapes:

- **`CoordError`** base class + all 15 subclasses (`AgentRequiredError`, `AgentNotHostedError`, `InvalidAgentError`, `InvalidFilenameError`, `MessageNotFoundError`, `InvalidStateError`, `InvalidPriorityError`, `InvalidDurationError`, `SyncFailedError`, `PeersConfigMissingError`, `PeersConfigInvalidError`, `EmptyBodyError`, `ArchiveConflictError`, `ResourceNotFoundError`, `InvalidResourceUrlError`, plus the legacy `InvalidIdentityError` alias) renamed to **`StError`** + subclasses. Public export from `@myobie/coord/errors`.
- **`createCoord()`** factory renamed to **`createSt()`**.
- **`Coord`** interface renamed to **`St`**.
- **`CoordOptions`** interface renamed to **`StOptions`**.
- **Wire meta key: `COORD_ERROR_META_KEY = 'coord/error'`** flipped to **`ST_ERROR_META_KEY = 'st/error'`** (silent flip per cos's steer — no embedder besides Nathan reading `_meta['coord/error']` today).
- **`resolveCoordBinPath()`** renamed to **`resolveStShimPath()`**. The redundant sibling helper `resolveStBinPath(coordBin)` (which took an already-resolved bin path and returned its `bin/st` sibling) has been dropped — `resolveStShimPath()` returns `bin/st` directly since #56.

Test refs bulk-updated:
- `_meta['coord/error']` → `_meta['st/error']` across integration + unit MCP tests.
- ~30 `Coord`-as-type refs across test files updated to `St`.
- `createCoord` call sites in embedding tests → `createSt`.

Full suite: 1555 pass, 3 pre-existing integration skipped.
Pre-push name-hygiene grep: clean. Updated the memory'd grep pattern to exclude the `@myobie/` npm scope — that's the actual package identity in `package.json` + import statements, not a personal-brand leak. All other `myobie` references (comments, test fixtures) are still caught.

### Changed (coord-kill piece (b) — CLI + env: `bin/coord`, `members`, `coord-` plugin prefix, and `COORD_*` env vars removed)

Post-cutover the CLI surface + env fallbacks are `st_*` only.

- **`bin/coord`** — deleted. `package.json:bin` no longer declares
  the `coord` entry.
- **`case 'members':`** CLI alias in `dispatchTop` removed.
  `st agents` is the sole canonical verb.
- **`coord-<cmd>`** — dropped from the git-style PATH-plugin scan
  in `findPlugin`. Only `st-<cmd>` and `smalltalk-<cmd>` prefixes
  are tried now.
- **`COORD_ROOT` / `COORD_IDENTITY` / `COORD_CONFIG` /
  `COORD_CHANNEL_DEBUG`** — no longer honored by any code path.
  `stRootFrom`, `stConfigFrom`, `envAgentFrom`, `resolveAgent`,
  and `cmdMcpCli` all read only `ST_*` env vars now. The
  `ST_IDENTITY → ST_AGENT` legacy alias (a smalltalk-era rename)
  is preserved with a one-time deprecation warning.
- **`coordRootFrom` / `coordConfigFrom` / `coordRoot` /
  `coordConfig`** helpers renamed to `stRootFrom` / `stConfigFrom`
  / `stRoot` / `stConfig` — ~200 call sites across `src/lib.ts`
  and every command module. `ResolveAgentOpts.coordRoot` field
  renamed to `stRoot`. `CliContext.coordRoot` → `stRoot`;
  `CliContext.coordConfig` → `stConfig`.
- **`warnCoordFallback`** internal helper renamed to
  `warnLegacyEnvFallback` (now only covers the
  `ST_IDENTITY → ST_AGENT` deprecation).
- **`invokedAsFrom`** now defaults to `'st'` (not `'coord'`); the
  `InvokedAs` type narrowed from `'coord' | 'st' | 'smalltalk'`
  to `'st' | 'smalltalk'`. `canonicalServerName` always returns
  `'st'`; `SERVER_INFO.name` is `'st'`; `buildServerInfo` accepts
  only `'st'`.
- **`~/.local/state/coord`** and **`~/.config/coord`** — no longer
  read as fallbacks. `defaultStateRoot` returns
  `~/.local/state/smalltalk`; `defaultConfigDir` returns
  `~/.config/smalltalk`.
- **`.mcp.json` legacy `coord` key** — no longer read or migrated.
  `cmdInit` only reads/writes `mcpServers.st`; a pre-cutover file
  with only a `coord:` entry is treated as absent (the `st` entry
  gets added; the `coord` entry is left alone).
- **User-visible verb strings** — bulk-scrubbed across source +
  hook scripts + tests: `coord ding` → `st ding`, `coord init` →
  `st init`, `` `coord X` `` in error messages → `` `st X` ``,
  `<coord-root>` in tool descriptions → `<st-root>`,
  `<channel source="coord">` phrasing removed from source
  comments. Codex hooks (`examples/codex/*.sh`) + Claude Code
  hooks (`examples/claude-code/hooks/*.sh`) rewritten st-only.

Piece (a)'s `EXPECTED_TOOL_NAMES` regression guard extended with
piece (b)'s reframed alias tests:
- `env-var resolution (ST_* only)` — regression guards that
  `$COORD_ROOT`, `$COORD_IDENTITY`, `$COORD_CONFIG` are NOT
  honored (each individually assertion).
- `state-dir resolution` — always resolves to
  `~/.local/state/smalltalk` even when `~/.local/state/coord`
  exists (regression guard).
- `bin/coord is REMOVED` — new source-guard regression assertion.
- `coord-<cmd>` plugin prefix NOT scanned — new regression
  assertion.
- `package.json bin` — asserts only `st` + `smalltalk` declared.

Cutover tools (`tools/cutover/rewrite-mcp-json.ts`,
`tools/cutover/rewrite-pty-toml.ts`, `tools/cutover/sweep.ts`) and
their tests removed — the migration they were built for is done.

Full suite: 1555 pass (3 skipped integration flakes).
Pre-push name-hygiene grep: clean.

Regression guard on source-level developer-path leaks in
`init.ts` broadened from a literal `/Users/<person>` match to a
generic `/Volumes/`, `/Users/`, and `/home/` catch — any absolute
developer-machine path fails the check.

Held on `feat/kill-coord-entirely` until the reboot signal
(Option B).

### Changed (coord-kill piece (a) — MCP tools registered under `st_*` only; dual-register removed; `members` alias retired)

Post-cutover the MCP tool surface is `st_*` only. The historical
dual-register mechanism has been fully removed:

- **`src/mcp/tools/dual-register.ts`** — deleted.
- **9 tool files** rewritten to call `mcp.registerTool('st_<name>',
  …)` directly.
- **`members` deprecated alias retired.** `st_agents` is the sole
  canonical name.
- **`CHANNEL_INSTRUCTIONS` fully rewritten st-only** — every
  `coord_*` tool name, `coord status` verb, `<channel
  source="coord">` reference, and "Coord threads stay on coord"
  convention line replaced.
- **`EXPECTED_TOOL_NAMES`** is `st_*` only (was 28-entry
  dual-prefixed; now 13-entry).
- Tool titles + descriptions scrubbed: no `coord message`,
  `$COORD_IDENTITY`, or `<coord-root>` phrasing remains.

Test coverage:
- `channel-instructions.test.ts` — st-only load-bearing
  substrings; added case-insensitive assertion that `coord`
  doesn't appear anywhere (cutover lock-in).
- `aliases.test.ts` Item 3 — reframed as "registry is `st_*`
  only"; asserts st_<base> present AND coord_<base> alias GONE.
- `mcp/lifecycle.test.ts` — EXPECTED_TOOL_NAMES array flipped.
- `mcp/context.test.ts` — dropped dual-register alias smoke.
- ~250 `coord_*` tool-name refs across 21 test files
  bulk-renamed to `st_*` (mechanical).

Load-bearing: first coord-kill piece touching the MCP wire
surface. Held on `feat/kill-coord-entirely` until reboot signal
(Option B).

Full suite: 1595 pass, 4 pre-existing flakes.

### Fixed (`st ding` session-flap debounce + PATH-robustness probe)

Two `st ding` hardening follow-ups from the same review that
produced #72 — both pre-reboot, both critical for
ding-as-primary-transport reliability.

**Fix 1: session-flap debounce.** A pty `--permanent` session is
auto-restarted by pty's supervisor. Between the old process's
exit and the new pidfile write there's a window where
`process.kill(pid, 0)` returns ESRCH, but the session is
actually about to come back. Without debounce, ding's session-
watch tick would trip the exit-when-gone path on that first
miss; the daemon exits; its own supervisor restarts it
eventually but arrivals during the gap are missed.

Fix: require `SESSION_GONE_DEBOUNCE_MISSES = 3` consecutive
"gone" observations before tripping. Any alive observation
resets the counter. A quick flap (1–2 ticks of "gone") rides
through cleanly; a real session death still trips within a few
ticks (~90s at the default 30s watch interval, but tests use
aggressive intervals to verify the math). Probe errors
(transient permission glitches, etc.) reset the counter too —
an unknown probe state shouldn't accumulate as evidence of
"gone."

Debounce logs the intermediate miss count so an operator can
grep the daemon's stderr for flap patterns: `target session
"<name>" appears gone (miss 1/3); debouncing before exit.`

**Fix 2: PATH-robustness probe at boot.** A ding daemon that
can't spawn `pty` runs forever with zero successful deliveries.
Especially load-bearing under a supervisor (launchd/systemd/
cron) whose environment strips PATH — the daemon comes up
"healthy" and silently drops every notice for its entire
uptime.

Fix: probe `pty --help` synchronously at the top of
`cmdDingCli` (after arg parsing + identity validation, before
starting timers/watchers). On ENOENT or non-zero exit, emit a
multi-line LOUD stderr banner (same shape as the hooks-loud
banner from #59), naming the specific probe failure and how to
fix it (typical fix: set PATH explicitly in the supervisor's
unit/plist). Return exit code 2 — refuse to start rather than
run forever with zero deliveries.

New exported helpers for testability:
- `probePtyOnPath(): PtyProbeResult` — the boot probe itself
- `cmdDingCli` accepts an optional third arg `{ ptyProbe?: () =>
  PtyProbeResult }` — tests inject a mock probe to exercise both
  the available and unavailable paths without shelling out

7 new unit tests:
- **Session-flap debounce**: single "gone" miss then alive again
  → ding stays running; N consecutive misses → ding exits;
  flapping (alternating gone/alive) never trips; probe error
  resets the miss counter (conservative).
- **PATH probe**: unavailable → cmdDingCli refuses to start with
  the LOUD banner + exit 2; available → cmdDingCli proceeds
  past the probe (no banner); `probePtyOnPath` happy path in
  the test env returns available.

Existing 65 ding tests unchanged and passing.

Full suite: 1596 pass, only the 4 pre-existing integration flakes.

Follow-up hardening findings NOT in this PR (deliberately scoped
down; documented as known / deferred):
- `isSessionAlive` false-positive on PID reuse — low-severity,
  documented behavior
- Cosmetic `coord ding:` log strings + unbounded buffer cap +
  SIGINT double-tap — fold into coord-kill piece (d)

### Fixed (CRITICAL — `st ding` hardening pass for primary-transport readiness)

With coord going away entirely and ding-mode becoming the DEFAULT
for `st launch`, `st ding` is now the PRIMARY inbox transport for
the whole network — not a fallback. On an MCP-hostile machine
(the operator's cos → supervisor → worker chain), a ding daemon
that crashes, silently loses messages, or garbles deliveries =
the whole setup fails silently.

A targeted hardening review found 3 blocker-severity gaps + 1
important (all with concrete production failure scenarios). This
PR closes them.

**Blocker 1: concurrent `pty send` calls were unserialized (race).**
Watcher-onEvent, buffer-flush drain, tidy tick, and startup scan
could all spawn `pty send` concurrently against the same session.
With `--with-delay 0.5` widening the per-send window to ~500ms,
text-A/text-B/return-A/return-B could interleave on the receiving
terminal — the first Enter committing A with B stuck in the paste
buffer, or A never committing because B's return already fired.

Fix: every `pty send` invocation goes through a per-daemon async
chain. The chain awaits the previous send's completion (regardless
of outcome) before invoking the next. Serialization is added at
the `send` reference used inside `runDing`, so all downstream
callers (deliver, tidy tick, etc.) get it automatically. Also
guards `tryFlush` re-entry (setInterval doesn't skip a tick when
the previous callback is still awaiting `deliver`) via a
`flushing` flag.

**Blocker 2: at-most-once semantics on `pty send` failure
(retry-semantics).** `deliver()` logged + dropped when `send` threw
OR returned non-zero. The operator's rule is at-least-once. A
transient pty error (target respawning, ECHILD/EPIPE, brief
supervisor hiccup) silently lost a notice.

Fix: `deliver()` returns a boolean (true on success, false on
failure). Callers requeue on false with an incremented `retries`
counter. Cap at `MAX_DELIVER_RETRIES = 5` per event — a
permanently-broken target drops the notice with a loud stderr line
("giving up on <fn> after 5 deliver attempts; check pty session
<name> or restart ding") rather than either dropping silently OR
blocking newer arrivals forever.

**Blocker 3: `buildEvent` read failures silently dropped
(retry-semantics).** `onEvent` at :404-410 caught read errors and
returned without buffering. Peer's atomic-rename race → read sees
mid-write → throws → notice lost. The nearby comment claimed
"lean toward delivering — better than silently dropping" but the
read branch violated it.

Fix: on read failure, buffer the bare filename in a new
`readPending` list. Each flush tick retries the reads; on success,
the event moves into the main buffer for delivery. Same
at-least-once guarantee as the send-retry path.

**Important 4: startup scan → watcher-arm race window.**
Historic ordering was `scan → arm watcher`. Files arriving BETWEEN
the readdirSync snapshot and the watcher arming were in neither
source — silently dropped on daemon startup. The relevant comment
even acknowledged the hole ("Files arriving DURING the scan are
out of luck").

Fix: arm the watcher's for-await loop FIRST (in a concurrent async
task), THEN run the scan. Both sources feed `onEvent`. `onEvent`
dedups via a `startupSeen` set for the first
`STARTUP_DEDUP_WINDOW_MS` (60 seconds — well beyond any real FS
watcher settle time). After the window closes the set is cleared;
the watcher is the sole event source from that point on.

8 new unit tests cover:
- Three concurrent arrivals → sender's peak concurrent-inflight
  count stays ≤ 1 (send serialization guard).
- Busy → available: 3 buffered notices flush without send overlap
  (flush-timer serialization guard).
- `pty send` fails once → requeues + delivers on next flush
  (retry-on-send-fail).
- `pty send` fails permanently → gives up after
  MAX_DELIVER_RETRIES with a loud log (retry cap).
- `coord.read` fails 1 time → filename buffered, delivers on next
  tick (retry-on-read-fail).
- `coord.read` fails 3 times → still delivers on the 4th tick
  (multi-retry).
- Watcher fires the same filename twice → delivered exactly once
  (startup dedup, direct).
- Two distinct filenames + one duplicate → 2 deliveries, dupe
  dropped (dedup precision).

Existing 57 ding tests unchanged and passing (regression-safe
refactor).

Full suite: 1589 pass, only the 4 pre-existing integration flakes.

Follow-ups (from the same review, not in this PR):
- Session-flap tripping exit-when-gone (debounce needed) —
  separate PR pre-reboot
- `spawn('pty')` PATH robustness (boot-time probe) — separate PR
  pre-reboot
- `isSessionAlive` false-positive on PID reuse — low-severity,
  documented as known behavior
- Cosmetic `coord ding:` log-string leaks + unbounded buffer +
  SIGINT double-tap → fold into coord-kill piece (d)

### Added (`st __launch-core` — hidden JSON-in/JSON-out entrypoint for the convoy bridge)

The launch write logic (identity resolution, `.mcp.json`,
session-id bootstrap, hooks, persona/DING-BUS install, pty.toml
emission, argv construction) currently lives in `src/commands/
launch.ts` — 2226 LOC of battle-tested TypeScript. When convoy
absorbs launch, it's a Swift codebase and can't import the TS
package as a library. Two-step cutover per convoy-claude:

1. **This PR (reboot moment)**: kill the `st launch` user
   surface (in the coord-kill branch), but keep the write logic
   in TS behind a stable hidden entrypoint convoy calls via
   subprocess.
2. **Fast-follow post-reboot**: convoy ports helpers to Swift
   one at a time, guarded by golden-file parity tests against
   this entrypoint's output. Subprocess drops when parity is
   green; smalltalk becomes pure bus.

Contract (stable, additive-only):
- **STDIN**: JSON body matching `LaunchInput` (minus `env` and
  `coordRoot`, which come from the invoker's process env).
  Unknown fields are IGNORED — forward-compat when older
  smalltalk sees a field a newer convoy sends.
- **STDOUT**: JSON body of `LaunchResult` on exit 0. Every
  written file's absolute path is enumerated on a `*Path` field
  (null when not written this launch).
- **STDERR**: error message on non-zero exit.
- **EXIT CODE**: 0 = success, 1 = validation error (input
  malformed, harness not `claude`/`codex`, bad field type), 2 =
  internal error (unexpected exception).

**Hidden**: NOT listed in `st help`, `st --help`, or shell
completions. Reachable only by name. This is not a user surface
— it's the convoy contract. Regression-guarded via a test that
asserts `st help` output does not mention `__launch-core`.

Design decisions:
- **JSON body over flags**: pure data contract; convoy sends
  only what it knows; adding a new launch input field never
  ripples flag-parsing changes into convoy.
- **Additive-only field renames** (same commitment as
  `AgentSummary.identity` → `agent`): field renames ship
  additive-then-deprecate so convoy's binder can be tolerant
  across releases.
- **Not a helper library**: the Swift/TS boundary forces
  subprocess-bridge; a "TS package API" is unusable from Swift.
  The subprocess is the pragmatic API.

15 new unit tests cover:
- Happy path: valid claude dry-run JSON → LaunchResult JSON on
  stdout, exit 0.
- Composition: `--ding` + `--fresh` layer correctly through the
  JSON (channel false, argv omits `--resume` + channels flag).
- Codex harness works too.
- Extra unknown JSON fields are ignored (forward-compat guard).
- Minimal input (just `harness`) uses defaults.
- Validation: non-JSON stdin, non-object JSON, missing/wrong
  `harness`, wrong-type `identity` / `ding`, extra positional
  argv — each emits a clear stderr message and returns exit 1.
- `--help` returns 0 with contract description on stderr;
  stdout stays clean.
- **Hidden regression guard**: `st help` output does NOT
  mention `__launch-core`.

Full suite: 1581 pass, only the 4 pre-existing integration flakes.

End-to-end smoke:
```
$ echo '{"harness":"claude","identity":"alice","ding":true,"fresh":true,"dryRun":true}' \
    | st __launch-core
{"identity":"alice","channel":false,"ding":true,"fresh":true,
 "argv":["claude","--permission-mode","bypassPermissions"], …}
$ echo $?
0
```

Ready for convoy-claude to wire the bridge ahead of the reboot.

### Fixed (CRITICAL — `st message reply` verb now exists; DING-BUS.md contract fulfilled)

Every ding-mode agent booting from `DING-BUS.md` (installed by
`st launch --ding` since #61) was instructed to run
`st message reply <filename> -m "<body>"` on inbox arrivals, but
the CLI dispatcher at `src/cli.ts:dispatchMessage` didn't route
`reply` — every agent hit `unknown subcommand: reply` on their
first response. Load-bearing gap that made ding-mode delivery
functional but reply-ability broken.

Fix:
- New `src/commands/reply.ts` with `cmdReply` (programmatic
  entry) and `cmdReplyCli` (CLI wrapper). Same locate + derive
  semantics as the MCP `st_msg_reply` tool:
  - Take `<thread-filename>` positional
  - Locate across `<self>/inbox`, `<self>/archive`, and every
    other identity's `archive/` (cross-identity case after sync
    mirrors a peer's archive back to the local tree)
  - Derive recipient from the thread's `from:` frontmatter
  - Derive default subject as `re: <original-subject>` (or omit
    if the original had none)
  - Body via `-m <body>` / `--message <body>` or stdin
    (mutually exclusive; the "both provided" case throws to
    prevent silent drops — matches `st message send` guard)
  - Optional `--subject S` override + `--from ID` sender override
- `case 'reply':` added to `dispatchMessage` (`src/cli.ts:165`)
- `messageUsage()` help text extended to include the new verb;
  top-level `st help` also lists `reply` alongside
  `send | ls | read | archive | thread`
- Shared `locateThread` extracted from `src/mcp/tools/reply.ts`
  into `src/locate-thread.ts` so CLI and MCP entry points can't
  drift on locate semantics (single source of truth)

14 new unit tests in `tests/unit/reply.test.ts`:
- Recipient derived from thread `from:`; reply lands in that
  identity's inbox with `in-reply-to:` + derived subject
- Locates thread in own archive (post-archive reply works)
- Locates thread in a peer's archive (cross-identity post-sync case)
- `--subject` override wins over derived default
- No subject in reply when parent had no subject
- Thread not found → `MessageNotFoundError`-shaped error
- Missing `$ST_AGENT` → clear error naming the env
- CLI `-m` inline body writes reply + prints filename to stdout
- CLI `--message` long-form works
- CLI `--subject` override
- CLI `--help` prints usage
- CLI no `<thread>` arg → clear error
- CLI `-m` + piped stdin → throws (matches `send.ts` guard)
- Regression: `st message reply <fn>` no longer errors with
  `unknown subcommand: reply` via the top-level `runCli` entry

Full suite: 1567 pass, only the 4 pre-existing integration flakes.

End-to-end smoke: `st message reply <fn> -m "<body>" < /dev/null`
writes into `<derived-recipient>/inbox/` with `from:` +
`in-reply-to:` + derived `subject:` frontmatter set. Ding-mode
agents can now reply per the DING-BUS.md contract.

Bug traced to #61 (DING-BUS.md install) — the contract documented
a verb that didn't exist. Discovered via a targeted `st --help`
+ completions audit (see follow-up PRs on the same audit's
completions binary-target and coverage-gap findings).

### Changed (`st launch` honors a direct `$PTY_ROOT` env verbatim; decoupled from `$ST_ROOT`)

Companion change to `--fresh`, bundled per cos (same code area).
Previously `st launch` DERIVED `PTY_ROOT = <ST_ROOT>/pty` and
IGNORED a directly-set `$PTY_ROOT`. Now:

- `$PTY_ROOT` set + non-empty in the invoker's env → use verbatim
- Else `$ST_ROOT` set (non-default network) → derive `<ST_ROOT>/pty`
  (Q2-A nested, unchanged from #68)
- Else → skip (default-network case, unchanged)

**One fix, two problems:**
1. **Eval stev-retirement cutover unblocked** — it needs a short,
   decoupled per-run pty root (`/tmp/stev-<runid>`) that the
   nested-derived form can't produce.
2. **Unix 104-byte socket-path limit sidestepped** — a nested
   `<ST_ROOT>/pty` under a deep sandbox path pushes pty's socket
   paths over the OS ceiling. A direct short `$PTY_ROOT` avoids it.

Emit is now independent of `ST_ROOT`: a launch with `$PTY_ROOT` set
but no `$ST_ROOT` emits `PTY_ROOT` alone (default bus, isolated pty
— the pure stev-retirement shape). The historic "matched-pair"
invariant from #68 is deliberately loosened for the decouple case;
the docstring on `buildPtyToml.opts.ptyRoot` names the precedence
rule.

4 new / updated tests:
- Nested-derive case (ST_ROOT only) → PTY_ROOT = ST_ROOT/pty (unchanged
  from #68; test description updated).
- **Direct $PTY_ROOT env** → baked verbatim, both agent + ding blocks;
  negative guard against the derived-nested form re-appearing.
- **Direct $PTY_ROOT without $ST_ROOT** → PTY_ROOT emitted alone,
  ST_ROOT absent (pure stev shape).
- **Empty-string $PTY_ROOT** → falls through to derive (regression
  guard on the null-ish check).

### Added (`st launch --fresh` — clean-slate session, no `--resume`)

New mode that skips the pinned-session bootstrap and OMITs
`--resume` from the launched argv. The agent starts with a
completely clean context and must rehydrate from durable state
alone (`now.md` + git + bus).

Mechanism for the **resumability eval's fresh-vs-`--resume`
A/B** — Arm D — and, per the larger roadmap, the mechanism for
eventually **dropping the session-id-resume ceremony**
entirely. If durable-state rehydration proves sufficient across
the eval, `--resume` becomes redundant and `convoy add` gets
simpler.

Semantics:

- **One-off, not a rewrite.** `--fresh` leaves any existing
  `.claude-session-id` / `.codex-session-id` file byte-for-byte
  untouched. The next non-fresh launch from the same cwd
  resumes the pinned session as usual. Reversible.
- **No jsonl bootstrap.** The one-shot `claude --print
  --session-id <SID> "session init"` that normally seeds the
  jsonl store is skipped — Claude Code auto-mints its own
  session on start.
- **Symmetric across harnesses**: `st launch codex --fresh`
  emits bare `codex` (no `resume <sid>`) even when a pin file
  exists.
- **Composes orthogonally** with `--ding`, `--persona`,
  `--permanent`, `--permission-mode`, `--agent`, etc. Fresh
  affects session-id + argv only.
- `LaunchInput.fresh?: boolean` + `LaunchResult.fresh: boolean`
  fields; `LaunchResult.claudeSessionIdPath` returns `null`
  under fresh mode.

Argv delta:

```
# Without --fresh (baseline)
claude --permission-mode bypassPermissions --dangerously-load-development-channels server:st --resume <SID>

# With --fresh
claude --permission-mode bypassPermissions --dangerously-load-development-channels server:st
```

11 new unit tests: default (no --fresh) has `--resume` (regression
baseline); `--fresh` omits `--resume`; live run doesn't write the
pin; live run with pre-existing pin preserves it byte-for-byte;
non-fresh live run still writes the pin (scoped-skip guard);
`LaunchResult.claudeSessionIdPath === null` under `--fresh`;
codex fresh skips reading `.codex-session-id` even when present;
non-fresh codex still uses `codex resume <sid>` (regression
baseline); `--fresh` + `--ding` compose orthogonally; CLI `--fresh`
threads through + dry-run summary; CLI default reports
`fresh mode: no`.

Full suite: 1550 pass, only the 4 pre-existing integration flakes.

Related: pty-claude may need a matching `pty-claude-launcher.sh`
option if their launcher hard-codes `--resume`; not blocking on
that — `st launch` builds argv directly and doesn't shell out to
the launcher script.

### Added (Phase-2 pty isolation: `st launch` emits `PTY_ROOT = <ST_ROOT>/pty` on non-default networks)

Set-side companion to pty-claude's Phase-2 PR (pty#55 — per-network
`PTY_ROOT` + `pty --root`). `st launch` now emits `PTY_ROOT` in the
pty.toml `[sessions.X.env]` block whenever the network is non-default,
matching the existing `ST_ROOT` env-line emit trigger byte-for-byte.

Derivation (Q2-A, **nested**): `PTY_ROOT = <ST_ROOT>/pty`. A network's
whole state (bus + pty) lives under one `ST_ROOT` dir, so `rm -rf
$ST_ROOT` removes the network entirely — the end-state Nathan
specified ("rm the folder, network's gone").

Semantics:
- **Matched-pair invariant with `ST_ROOT`.** `PTY_ROOT` emits if and
  only if `ST_ROOT` does. A bare-bus / shared-pty split would defeat
  the "rm the folder" semantic; the pair moves in lockstep.
- **Both session blocks tagged**: main session + ding sidecar env
  blocks each carry the same `PTY_ROOT` value. Consistent with the
  existing `ST_ROOT` mirror.
- **Default network unchanged**: no env line emitted; pty.toml
  doesn't freeze today's default into future restarts. Same
  asymmetry-with-`st.network`-tag preserved.
- **Legacy `COORD_ROOT`**: canonicalized to `ST_ROOT` via the
  existing input.coordRoot resolution, then flows through the
  PTY_ROOT derivation — no separate legacy path; env line name is
  the post-cutover canonical `PTY_ROOT` regardless of which env
  the invoker used.

Post-merge, an agent launched into a non-default network gets both
bus + pty isolation for free — an evals-side follow-up to retire
stev's session-prefixing becomes possible.

4 new unit tests: default network → no `PTY_ROOT` emitted; explicit
`ST_ROOT` → both `ST_ROOT` and `PTY_ROOT = <ST_ROOT>/pty` present;
codex ding sidecar carries `PTY_ROOT` too (matched-pair with
`ST_ROOT`); legacy `COORD_ROOT` invoker env produces the canonical
`PTY_ROOT` shape (no `COORD_PTY_ROOT` regression).

Full suite: 1539 pass, only the 4 pre-existing integration flakes.

### Fixed (`st launch` git-exclude append now works in git worktrees)

Historic behavior: `readGitExclude(cwd)` did
`join(cwd, '.git', 'info', 'exclude')` and stat-checked `.git`
as a dir. In a git **worktree** (created via `git worktree add
<path>`), `<worktree>/.git` is a text FILE pointing at
`<main>/.git/worktrees/<name>` — the dir check returned false,
`readGitExclude` returned null, and every downstream caller
surfaced `gitRepoAbsent: true`. The persona files (PERSONA.md,
DING-BUS.md, .mcp.json, generated CLAUDE.md, session-id files,
pty.toml) were still installed but silently NOT excluded from
git tracking — an operator working out of a worktree
accidentally staged them.

Fix: use `git rev-parse --git-path info/exclude`. Git resolves
this correctly for every layout:
- Regular repo → `<cwd>/.git/info/exclude`.
- Worktree → the SHARED `<main>/.git/info/exclude` (info/exclude
  is shared across worktrees per git's design).
- Bare repo → the repo's `info/exclude`.
- Non-git dir → nonzero exit → `readGitExclude` returns null (the
  existing `gitRepoAbsent: true` surface stays intact for
  callers).

The write path (`appendGitExclude`) already used the returned
absolute path directly, so it flows through unchanged — the fix
lives entirely inside `readGitExclude`.

Tests:
- New positive worktree regression guard: launch in a worktree
  cwd → git-exclude entries land in the MAIN repo's shared
  `.git/info/exclude`, and the worktree's `.git` is confirmed to
  be a FILE (not a dir) we didn't touch.
- Existing `makeGitRepo(dir)` fixture updated from `mkdir -p
  .git/info/` (which no longer satisfies `git rev-parse`) to a
  real `git init` — 12 persona tests now boot a real repo, more
  faithful to production.
- The dry-run "touches nothing" assertion updated: `git init`
  ships with a default `.git/info/exclude` template, so the
  test now checks the exclude file (if present) contains
  neither our appended `PERSONA.md` line nor the
  `smalltalk-launch` block header — a more precise "we didn't
  write to it" guarantee than the historic "the file doesn't
  exist" assertion.

Full suite: 1532 pass, only the 4 pre-existing integration flakes.

### Docs (`notes/onboarding.md` — add SHA-pin guidance for the personas checkout)

Onboarding step 1 says `git clone https://github.com/myobie/
personas`, which gives you a rolling-HEAD checkout. Fine for
solo work following current guidance, but a problem for:

- Evals: reproducibility across runs requires the persona files
  to match the ones the eval was designed against.
- Workshops / teaching: everyone should be on the same persona
  content so the walk-through matches what participants see.
- Shared team CoS setups: if two teammates clone at different
  times, they end up with subtly different CoS behavior.

Added a note right under the clone block explaining the
tradeoff and showing how to pin (`cd personas && git checkout
<sha>`). Also names the currently-tested set (`96a6331`, the
personas HEAD as of 2026-07-06) so readers evaluating smalltalk
against this doc get the exact files the doc was validated
against. Includes a maintenance instruction to bump the SHA
when the tested set advances — a rolling HEAD in the
instructions plus a personas change would silently drift.

### Fixed (`DING-BUS.md` now tells the agent to propagate ding-mode through every spawn)

evals-claude caught a contract-level gap in DING-BUS.md before
its live run even finished: a ding-mode CoS following its role
("stand up a specialist per repo") had **zero instruction to add
`--ding`** to the children it launches. On an MCP-capable box
the mixed-mode tree still completes → false-pass; on Johannes's
actual MCP-hostile setup the child fails to boot and the whole
cos → supervisor → worker chain collapses.

Root cause: the shipped `DING-BUS.md` (from #61) covered
boot-ritual, `[DING]`-poke handling, threads-on-bus, and a
CLI inventory — but nothing about *spawning children*. The
`st launch` command wasn't even in the inventory. A ding-mode
agent following the contract would faithfully copy its own
launch pattern from the persona / onboarding docs — where the
examples are `st launch <harness> …` without `--ding` — and
silently produce an MCP-mode child.

Fix — add a new "Propagate ding-mode through every spawn"
section to `DING_BUS_INSTRUCTIONS`:

- **Rule stated plainly**: "ding-mode is not a per-agent choice
  — it's a property of the whole machine, so every agent you
  spawn on this machine MUST also be in ding-mode."
- **Exact command shape**: `st launch <harness> --identity
  <child-id> --ding [--persona <path>] [--permanent] …`
- **Cascade guarantee**: the child gets its own DING-BUS.md
  automatically (since `st launch --ding` installs the same
  contract in the child's cwd), so the rule holds at every level
  of a cos → supervisor → worker tree.
- **Anti-pattern explicit**: "Do NOT run plain `st launch`" —
  names the failure mode (spawns an MCP-mode child that fails to
  start, or worse, appears to start and delivers nothing).
- **Copy-paste warning**: "you must add `--ding` to any copied
  `st launch …` command that doesn't already have it" — targets
  the exact behavior that produced the false-pass.
- **CLI inventory expanded**: `st launch <harness> --identity
  <id> --ding [...]` now listed under a new "Spawning children"
  block, cross-referencing the rule above.

CHANNEL_INSTRUCTIONS deliberately does NOT mirror this section —
MCP-mode has no equivalent constraint (an MCP child on an
MCP-capable machine is fine). The `DING_BUS_INSTRUCTIONS`
docstring calls out the asymmetry so a future reader doesn't
add a matching-but-nonsensical section to the MCP side.

Tests:
- New positive regression guards in the existing DING-BUS.md
  content test: the new section header, the rule statement, the
  exact `st launch <harness> --identity <child-id> --ding`
  command shape, the "Do NOT run plain `st launch`"
  anti-pattern, and the new "Spawning children" CLI-inventory
  block are all asserted present.
- Existing negative guards (no `coord` leak, etc.) still fire.

Load-bearing for Johannes's team — this is what makes his
cos → supervisor → worker chain work end-to-end on the
MCP-hostile machine, not just the top-level CoS.

Full suite: 1531 pass, only the 4 pre-existing integration flakes.

### Added (Phase-1 pty isolation: `"st.network"` tag on every `st launch` session)

pty-claude designed the tag; this PR wires it into `st launch`.
Every session emitted into pty.toml — the agent block AND
(when present) the ding sidecar block — now carries an
`"st.network" = "<value>"` tag. The tag is a uniform inspection
signal that separates smalltalk-network sessions from an
operator's ad-hoc pty use: **presence = "this is a smalltalk-
network session"; value = which network.** pty's
`--filter-tag st.network=<value>` primitive filters on it out of
the box (verified against `../pty/src/tags.ts:matchesAllTags` +
smol-toml's quoted-key parsing).

Design decisions per pty-claude's steer:

- **Value = the resolved network root** (`input.coordRoot`, which
  cmdLaunch already computes via `coordRootFrom(ctx.env)` at the
  CLI layer). Always a valid path regardless of whether the
  invoker set `ST_ROOT` explicitly or is on the default.

- **Emit for EVERY st-launched session, including the default
  network** — deliberately unlike the `ST_ROOT` *env* line (which
  omits itself on the default network to avoid freezing today's
  default into tomorrow's restarts). The tag is a pure inspection
  label, so uniformity means the presence-check is the signal.

- **Key spelled exactly `st.network`** (pty-claude's choice —
  visible, non-reserved in `../pty/src/tags.ts:EXACT_RESERVED`).
  The dot is inside a quoted TOML inline-table key
  (`"st.network" = "..."`) so it's not interpreted as a dotted
  nested-table key. Verified live against smol-toml parsing.

- **Matched-pair invariant**: same value in the agent block and
  the ding block. A launch is one network; the sidecar carries
  the same tag as the main session. Matches the same "agent +
  ding stay in sync" invariant the `agentStrategy` mirror
  enforces.

3 new unit tests cover:
- Isolated launch (explicit `ST_ROOT`) → both agent + ding blocks
  tagged with the same value.
- Default launch (no env) → still tagged (uniformity regression
  guard), with `ST_ROOT` env line still absent (asymmetry
  preserved).
- Claude launch (no ding sidecar) → single-block launch still
  tagged, agent-only.

Existing 5 tag-shape assertions updated from strict-form
`toContain` to regex `toMatch` — the new emitted form includes
the network tag suffix, and the regexes lock in the "no strategy
between role and st.network" semantic for the ephemeral case AND
the "strategy = permanent between role and st.network" semantic
for the permanent case. Full suite: 1534 pass.

### Fixed (CRITICAL — `st ding` sidecar command targets the FQN pty session, not the bare `sessionName`)

evals-claude's end-to-end confirmation run of the Johannes stack
caught a SECOND `--ding` bug that #62's startup-grace was
masking: the ding sidecar was addressing the WRONG pty session
name → every `[DING]` poke returned `Session "<x>" not found`
→ delivery was silently broken even though survival looked
healthy.

Root cause: `buildPtyToml` composed the ding sidecar command as

```
st ding <sessionName> --identity <identity>
```

using the BARE `sessionName` — same value that goes into the
`[sessions.<sessionName>]` TOML block. But pty joins `prefix +
sessionName` with a dash (see `../pty/src/ptyfile.ts:58`, the
same convention the F1 auto-poker dash-fix landed on), so the
FQN pty session name is `<identity>-<sessionName>`. Addressing
the bare form silently mis-hits every poke.

Example failure mode: `st launch claude --identity cos --ding`
→ pty.toml wrote `st ding claude --identity cos`, but the actual
pty session key was `cos-claude`. Every poke `pty send "claude"`
returned "Session not found".

**#62 masked it perfectly:** the startup-grace waited for the
bare name to appear, which never happens (real name is
prefixed), so the sidecar survived forever LOOKING healthy while
delivering nothing.

Fix: one-line change at the ding-command build site — target
`${opts.identity}-${opts.sessionName}` instead of bare
`${opts.sessionName}`. Matches the F1 auto-poker at the same
function (which already uses the FQN form, correctly, since the
dash-fix landed).

Class audit: this is the SECOND session-name-addressing bug in
`launch.ts` (F1 poker slash-vs-dash was the first). Grepped all
send-target sites: only the ding command line had the bug — the
poker uses the FQN form, and the `[sessions.NAME]` blocks are
structural declarations (pty prepends the prefix automatically).
`src/commands/ding.ts` sends whatever it's told; the bug was
purely in what launch.ts passed as the CLI arg.

- **Positive regression guard**: existing `st ding <target>`
  assertions updated to the FQN form. Every claude ding-mode +
  codex launch now asserts the identity-prefixed target.
- **Negative regression guard**: the bare form is explicitly
  negated with `.not.toMatch` so a future refactor can't revert
  to the mis-addressed shape.
- **New scenario test** covering the eval case cos cited:
  identity `dm-dev` + custom `--session-name` → ding target must
  be the identity-prefixed form, not the bare sessionName.

Full suite: 1531 pass, only the 4 pre-existing integration flakes.

Unblocks:
- Johannes's ding-mode CoS delivery end-to-end (#62 unmasked
  this; now delivery actually works).
- The eval suite's ding-mode delivery (evals-claude's confirmation
  re-run can now truly close the loop).

### Fixed (CRITICAL — `st ding` startup grace: sidecar no longer dies on the launch race)

evals-claude's live ding-mode run caught a critical defect that
was blocking Johannes's CoS AND the whole eval suite's
ding-mode: the `st ding` sidecar was dying at launch, never
delivering a single `[DING]` poke, and — being ephemeral (per
the #54 fix) — never restarting. Ding-mode agents booted, found
empty inboxes, and idled forever ("watching for the [DING]"
while delegations sat unread).

Root cause: the session-watch tick's default `--exit-when-
session-gone` behavior fired on the FIRST tick, which typically
races AHEAD of pty's registration of the target agent session.
The tick saw "target gone" → aborted → daemon exited.

Fix (cos-approved semantic): **startup grace** — only trip the
exit-when-gone path AFTER the ding has seen the target alive at
least once. A not-yet-appeared target is treated as "still
launching", not "gone", and doesn't trigger exit.

- New internal `seenTargetAlive` flag on `runSessionWatchTick`,
  false at daemon start. Flips to true on the first `alive ===
  true` observation. The exit branch is now gated on both `alive
  === false` AND `seenTargetAlive === true`.
- Startup-grace log line ("target session … not yet registered;
  waiting for it to appear before enabling the exit-when-gone
  watch") fires ONCE, gated by a `loggedWaitingForTarget` flag —
  no per-tick spam.
- Once the grace is cleared (target became alive), the exit path
  works as before — a REAL "session ended" transition still
  aborts. The grace is a startup shield, not a persistent one.

Test coverage:
- **Startup-grace test**: session is dead from launch → daemon
  waits, does NOT exit. Regression guard on both the log
  (waiting-line fires exactly once, no per-tick spam) and the
  daemon lifecycle (`r.done` has not resolved after several
  ticks).
- **Grace-clears test**: dead → alive → dead still exits — proves
  the grace is transient and doesn't mask real session-ended
  signals.
- Existing 4 session-watch tests still pass unchanged (the
  "session goes away" case uses a scenario where alive=true is
  observed BEFORE flipping to false, so the grace clears
  correctly).

No API changes. No new flags. Behavior automatic for every
`st ding` daemon.

Johannes-blocking; unblocks ding-mode delivery end-to-end.

### Added (`st launch --ding` installs `DING-BUS.md` — bus-mechanics contract for ding-mode agents)

Final step of the Johannes stack. Ding-mode agents have no MCP
transport → no MCP `instructions:` blurb (the boot ritual /
channel-notification-handling / tools-inventory contract every
MCP agent gets on connect). Without an equivalent, a bare
ding-mode CoS launches without knowing the CLI flow, how to
handle `[DING]` pokes, or the "threads stay on the bus"
convention. This close makes ding-only genuinely first-class,
not degraded.

Fix (cos-approved option 2):

- **`DING_BUS_INSTRUCTIONS`** exported constant in `launch.ts` —
  the ding-mode analog of `src/mcp/capabilities.ts:CHANNEL_INSTRUCTIONS`.
  Content structured to mirror MCP's shape: boot ritual →
  `[DING]`-poke handling → threads-stay-on-bus convention →
  CLI inventory. Explicitly notes ding-mode agents will NOT
  receive `<channel>` blocks (per cos's refinement — an agent
  shouldn't wait for something that never comes).

- **`installDingBusInstructions()`** mirrors `installPersona()`:
  writes `<cwd>/DING-BUS.md`, surgically appends `@DING-BUS.md`
  to `CLAUDE.md` (creates the file if absent; leaves a
  pre-existing repo `CLAUDE.md` in tracking), adds `DING-BUS.md`
  to `.git/info/exclude`. Idempotent on re-run.

- **Auto-installs with `--ding`** on the claude harness — ding-mode
  is claude-only (codex has its own instructions path).
  `LaunchResult.dingBus: DingBusInstallResult | null` for
  observability + a new `ding-bus:` block in the `--dry-run`
  summary.

- **Cross-reference comment** on both `CHANNEL_INSTRUCTIONS`
  (`src/mcp/capabilities.ts`) and `DING_BUS_INSTRUCTIONS`
  (`src/commands/launch.ts`) marking them as "two versions of one
  bus contract — keep in sync when the contract changes." Drift
  means MCP agents and ding-mode agents will behave differently
  for the same protocol event.

- **`notes/onboarding.md`** section 2 gains an "Alternative:
  `--ding` (MCP-hostile environments)" subsection covering when
  and why to use it, the full launch command, and what
  auto-installs on top of the persona.

9 new unit tests cover: no-`--ding` case leaves `dingBus` null;
`--ding` populates the result on dry-run; live-run writes
DING-BUS.md with the load-bearing substrings + confirms no `coord`
leak; `@DING-BUS.md` composes with `@PERSONA.md`; git-exclude
happens; codex + `--ding` is a no-op on the install path;
missing-git-repo warning fires + DING-BUS.md still installs;
idempotent re-run doesn't duplicate the import line; CLI
`--dry-run` summary shows the `ding-bus:` block.

### Changed (ding-delivered messages now prefixed with `[DING] ` + `smalltalk` naming)

Two `st ding` delivery paths — the inbox-arrival notice and the
tidy-check drift summary — now prepend `[DING] ` and refer to
"smalltalk message" / "tidy-check" (not "coord message" / "coord
tidy-check").

Rationale (per Nathan):

- The prefix marks the line as bus traffic, visually distinct
  from three other things an agent might see in its terminal:
  MCP `<channel source="…">` blocks (MCP path only), the agent's
  own REPL output, or a human typing at the REPL.
- The prefix lets a ding-mode agent's persona (and the upcoming
  `DING-BUS.md` blurb) reference an unambiguous string pattern
  in the poke-handling flow ("when you see `[DING] X`, do Y").
- `smalltalk`/`st` naming aligns with the CLI (`st message …`)
  the agent uses to act on the notification. `coord` in a
  first-impression, user-visible line was a rename leftover.

Concretely:

- `you have a new coord message: <subject> (from <sender>);
  check your inbox` → `[DING] new smalltalk message: <subject>
  (from <sender>); check your inbox`
- `coord tidy-check: inbox=<n> (oldest <age>).` →
  `[DING] tidy-check: inbox=<n> (oldest <age>).`

Test coverage:
- Existing 8 assertion sites updated across `tests/unit/ding.test.ts`
  and `tests/integration/ding.test.ts` to the new form.
- New positive regression guards: `[DING] ` prefix required,
  legacy `coord message` / `coord tidy-check` forms negated —
  so a future refactor can't revert to the un-prefixed +
  legacy-named form.

Internal daemon log lines (`coord ding: pty send failed: …`,
etc.) are unchanged — those are stderr for operators, not user-
visible bus traffic. Cosmetic rename can happen alongside a
broader daemon-log cleanup later.

### Fixed (`st launch` hooks-not-found is now LOUD instead of a silent soft-skip)

Historic behavior: when `resolveClaudeHooksDir()` returned null
(bin/shim not resolvable OR `examples/claude-code/hooks/` not on
disk OR explicit `--hooksDir` override missing), the call site
emitted a single line — `[smalltalk launch] shipped Claude Code
hooks not found on disk; skipping .claude/settings.local.json` —
and the launch proceeded HOOKLESS. Silent-ish → an operator
missed it → Johannes's claude came up without the boot ritual,
PreCompact flush, or StopFailure ding, exactly the silent-install-
gap class of bug the `bin/coord` rename fix already surfaced.

Two changes:

- **`resolveClaudeHooksDirWithHint()`** (new; discriminated variant
  of `resolveClaudeHooksDir()`) categorizes the failure into
  distinct modes and returns a `hint` string quoting the path(s)
  that were inspected and how to fix each mode:
  - bin/shim resolution failure → names the package.json walk +
    `which st`/`which coord` PATH lookup failure.
  - `examples/claude-code/hooks/` missing → quotes the walked-to
    root, suggests `npm install && npm link` from the checkout, or
    `--no-hooks` to acknowledge intentionally.
  - Explicit `--hooksDir` override missing → names the specific
    path passed.

- **Multi-line LOUD stderr banner** at the call site. Replaces the
  historic one-line notice with a bracketed block naming what's
  disabled (boot ritual, PreCompact flush, StopFailure ding), the
  specific failure hint, and how to fix it or silence intentionally
  with `--no-hooks`. Launch still proceeds (hookless is degraded,
  not fatal) but the operator gets a signal they can't miss on
  scroll-back.

`resolveClaudeHooksDir()` retained as a thin wrapper around the
new function for back-compat with test/embedder callers.

Test regression guards:
- Historic silent one-line message form is negated (`.not.toMatch`)
  so a future refactor can't revert to it.
- The banner + the actionable "why" line quote the missing path.
- The `--no-hooks` escape hatch is called out in the guidance.
- New unit test for `resolveClaudeHooksDirWithHint()` happy path in
  the repo checkout returns real hooks dir + null hint.

Also negates the specific override failure form vs. the auto-
resolution form so a diagnostic-message copy/paste regression is
caught.

### Added (`st launch claude --ding` — codex-style ding-mode for MCP-hostile environments)

Load-bearing for Johannes's setup, where MCP servers can't run
at all. Without this flag `st launch claude` requires a working
MCP transport (`.mcp.json` + `--dangerously-load-development-
channels`); with `--ding`, the same claude agent joins the
network the way codex does — via an `st ding` sidecar delivering
inbox pings + the `st` CLI for bus ops (send/ls/read/archive/
reply). No MCP.

What `--ding` changes on a claude launch:

- **Skips `cmdInit`** — no `.mcp.json` written. The MCP-hostile
  environment couldn't spin up the server anyway.
- **Forces `channel = false`** — no `--dangerously-load-development-
  channels server:st` in the argv (there's nothing to load).
- **Adds the `st ding` sidecar** to the generated `pty.toml`,
  same shape as `st launch codex`. `st ding <sess> --identity
  <id>` watches the identity's inbox and `pty send`s
  notifications into the agent's terminal.
- **Hooks stay generated** — the boot ritual, PreCompact flush,
  and StopFailure ding are Claude Code hooks, MCP-independent.

Stale `.mcp.json` from a prior MCP-mode launch triggers an
advisory stderr warning ("Delete it (rm .mcp.json) if this was a
switch from MCP-mode to ding-mode") — Claude Code will still
read a stale file even in ding-mode, so cleanup is on the
operator.

`--ding` on `codex` is a no-op (codex is already ding-mode by
default — `addDingSidecar` fires unconditionally for that
harness). The flag exists on the codex path for shell-history
symmetry; the ding sidecar count guard test locks in "no double-
write".

9 new unit tests cover: default (no --ding) preserves MCP wiring
(byte-identical regression guard); --ding claude → no channel
flag + ding sidecar; --ding claude on a live run doesn't write
`.mcp.json`; non-ding claude still writes `.mcp.json` (scoped-
skip regression guard); stale `.mcp.json` advisory warning;
--ding codex no-op (single ding session, not double-written);
hooks still generate under --ding; CLI plumbing + dry-run
summary (both branches).

### Fixed (spawner-shaped launches default to `bypassPermissions` + guard warning extended to supervisor)

Nathan's 3-tier permission model, closing Johannes's pty.toml Bug 1:

- **cos + supervisor** are spawners → default `bypassPermissions`
  when neither `--permission-mode` nor `$CLAUDE_PERMISSION_MODE`
  is set (was `auto`, which claude's classifier hard-blocks from
  spawning autonomous agents — the exact regression Johannes hit).
- **workers** (any other identity + no spawner persona) → default
  `auto`. `auto` is correct + safe for a leaf agent that does
  work but doesn't spawn.

Spawner detection matches previous PR's CoS-shape (`identity ===
'cos'` OR persona basename === `'chief-of-staff.md'`) extended
with `identity === 'supervisor'` OR persona basename ===
`'supervisor.md'`. Both persona files live at
https://github.com/myobie/personas (HEAD `b8a2cc3`).

Also extended the previous PR's footgun-guard warning:

- Fires for both `cos` AND `supervisor` (was CoS-only).
- Message updated: `"launching a spawner (cos/supervisor) without
  --permanent"` (was `"launching a CoS"`).
- Ephemeral eval spawners intentionally decline `--permanent`
  (they need to be reap-able on teardown) — the warning is
  opt-in acknowledgment, not a hard block.

Deliberate asymmetry (per Nathan): the permission-mode default
flip is safe (spawners need bypass, evals want bypass), but
permanent stays opt-in / warn-only — evals launch `--identity
supervisor` agents that MUST stay ephemeral.

Precedence at the CLI: `--permission-mode <mode>` >
`$CLAUDE_PERMISSION_MODE` env > shape-aware default (spawner →
bypass, worker → auto). Existing callers with explicit flags or
env set are byte-identical.

`notes/onboarding.md` updated: CoS launch command in step 2 adds
`--permission-mode bypassPermissions` for pattern-teaching (works
either way — the default now covers it, but the explicit flag
teaches the pattern you'll want on your own supervisor launches).
Resume recipe + the aliased-binary example also updated.

11 new unit tests cover: cos + supervisor identity defaults, both
persona basenames as detection triggers, worker stays on auto,
explicit flag overrides the spawner-default, `$CLAUDE_PERMISSION_MODE`
overrides too, supervisor warning fires + worker doesn't, and
supervisor + --permanent = no warning but still bypass.

### Fixed (`st init`/`st launch` emit `bin/st` + `st ding`, not the legacy `bin/coord` + `coord ding`)

Two rename leftovers from the coord→st cutover that were producing
config files pointing at the legacy shim name:

- **`.mcp.json`'s MCP server command**: `st init` (and the `st
  launch` path that fans out through it) had
  `resolveCoordBinPath()` returning `<package-root>/bin/coord`,
  which then landed as the `.mcp.json` `st` entry's `command:` on
  every fresh launch. Worked (bin/coord is a dual alias for bin/st)
  but perpetuated the drift — the day the coord alias is dropped
  would break every generated `.mcp.json`.

- **`pty.toml`'s ding sidecar command line**: `st launch codex`
  emitted `command = "coord ding <sess> --identity <id>"` for the
  ding session. Same dual-alias mechanic; same drift risk.

Both fixes:

- `resolveCoordBinPath()` now walks for and prefers `<pkg>/bin/st`,
  falling back to `<pkg>/bin/coord` only for very-old installs
  where the newer shim isn't present. PATH fallback is `which st`
  first, then `which coord`. Function name kept for now (callers-
  are-me-only; a rename is scope-creep on the fix).
- `buildPtyToml`'s codex-ding line changed to `st ding <sess>
  --identity <id>`.
- Small doc + comment updates in `launch.ts` (`coord ding sidecar`
  → `st ding sidecar` in LAUNCH_HELP + the module-level comment).

Test regression guards added:
- init.test.ts asserts `resolveCoordBinPath()` returns
  `.../bin/st` AND explicitly negates `.../bin/coord`.
- launch.test.ts asserts the codex ding preview contains `st ding`
  AND explicitly negates `coord ding`.

Users regenerate stale files by removing them (`rm .mcp.json
pty.toml`) and re-running the corresponding launch. Existing
`.mcp.json` / `pty.toml` files with the legacy names keep working
until the coord alias is dropped; no forced migration.

### Added (`st launch --permanent` for durable CoS-shaped launches + footgun-guard)

Closes the CoS-permanence gap the previous PR (ephemeral-ding fix)
surfaced: `st launch claude --identity cos` produced an agent with
`tags = { role = "agent" }` — no strategy tag — which pty treats
as its ephemeral default. `pty gc` would reap the CoS along with
any idle session. That's a silent, non-obvious failure mode for
the always-on center a newcomer just followed the onboarding docs
to stand up.

Fix:

- **`--permanent` CLI flag on `st launch`.** Sets
  `agentStrategy = "permanent"` on the buildPtyToml call, which
  the previous PR's future-proofing hook plumbs into BOTH the
  agent session AND (for codex) the ding sidecar. A launch is
  either permanent or it isn't; no mixed-strategy pty.toml. pty
  sees `strategy = "permanent"` and treats the session as
  durable (see `../pty/src/sessions.ts:576, 620, 644`).

- **Footgun-guard warning.** When a CoS-shaped launch omits
  `--permanent`, stderr gets:

  ```
  [smalltalk launch] launching a CoS without --permanent; pty gc
  may reap it under idle-cleanup. If this is a production CoS
  (not an eval/test spin-up), pass --permanent so the launch
  bakes strategy = "permanent" into pty.toml.
  ```

  Detection is narrow: `identity === 'cos'` OR the persona
  basename is `chief-of-staff.md`. Ephemeral eval launches,
  workers, specialists get no warning.

- **`LaunchResult.permanent: boolean`** for observability; new
  `permanent:     yes|no` line in the `--dry-run` summary.

- **`notes/onboarding.md` updated.** CoS launch command in
  step 2 now includes `--permanent`. Resume recipe includes
  `--permanent`. Troubleshooting entries added for the warning
  and the reap-able-CoS failure mode.

- **`LAUNCH_HELP`** documents the flag; a new example line
  demonstrates the full production-CoS invocation
  (`--identity cos --permanent --persona chief-of-staff.md`).

9 new unit tests cover: default no-tag/no-strategy, --permanent on
agent, --permanent on codex matches ding (invariant guard), CoS
footgun-guard by identity, CoS footgun-guard by persona basename,
--permanent silences the warning, non-CoS identity gets no
warning, CLI --permanent threads into dry-run summary, CLI
default reports `permanent: no`.

### Fixed (`st launch codex` no longer bakes `strategy = "permanent"` on the ding sidecar)

Historic behavior: `buildPtyToml` hardcoded
`tags = { role = "ding", strategy = "permanent" }` on the codex
ding sidecar while the agent session itself carried no `strategy`
tag — a mismatch that meant `pty gc` after an ephemeral codex run
died would resurrect the ding as a zombie, exactly what the
ephemeral-eval rule forbids. Blocked the 4 codex-cell eval
retrofits.

Verified against `../pty/src/sessions.ts:576, 620, 644` that pty
only checks `strategy === "permanent"` explicitly — absence of the
tag is pty's ephemeral default. So the fix is to drop the
hardcoded `permanent` on the ding: `tags = { role = "ding" }`. Now
both the agent AND the ding are ephemeral by default; `pty gc`
reaps them together on eval teardown.

Future-proofing hook: `buildPtyToml` grew an
`agentStrategy?: string | undefined` opts field. When set, the
value is mirrored to BOTH the agent and ding tags — a launch is
either ephemeral or it isn't; no more mixed-strategy pty.toml. No
CLI flag exposes this yet; a follow-up `--permanent` (for a
production CoS that needs to survive `pty gc`) will plumb through
here.

Impact:
- Codex ephemeral evals: unblocked. Ding no longer zombies.
- CoS use case: unaffected. CoS runs the claude harness with no
  ding sidecar (see `addDingSidecar: harness === 'codex'`).
- Codex users who relied on ding-permanence to survive a codex
  restart: behavior changes — a `pty gc` after codex dies now
  reaps the ding. `pty up` brings both back. Note: pty gc would
  ALSO have zombied that ding when the whole session tree died,
  so the current permanent tag was arguably a bug either way.

Test coverage:
- Historic assertion `expect(preview).toContain('strategy =
  "permanent"')` → negative assertion (regression guard).
- New positive assertion: ding tags are exactly
  `{ role = "ding" }`.
- New positive assertion: agent tags remain `{ role = "agent" }`
  (regression guard for the historic bare-agent shape).

### Docs (onboarding.md: fix the missing persona bootstrap in the CoS quickstart)

The CoS quickstart's step 2 said `st launch claude --identity cos`
— which produces a bare Claude Code agent that has no idea it's a
CoS. Step 3 then said "the CoS consumes the personas repo on boot"
— but nothing in the actual launch command told the agent to do
that, so the docs described a bootstrap that doesn't self-start.
Follow the recipe literally and you get generic Claude in a
folder.

Fix per Nathan's framing — you make the repo, THEN launch the CoS
IN it:

1. **Step 1 (Install)** — extends the side-by-side clone list to
   include `myobie/personas` alongside `pty` and `smalltalk`, so
   the persona file is on disk before step 2 needs it.
2. **Step 2 (Bring up your CoS)** — makes the ordering unambiguous
   (mkdir the cos folder → `git init` → launch INSIDE it) and adds
   the load-bearing `--persona
   ~/src/github.com/myobie/personas/chief-of-staff.md` argument.
   Documents what `st launch --persona` actually does (copy to
   `PERSONA.md`, wire `@PERSONA.md` into `CLAUDE.md`, git-exclude
   both). Notes the aliased-binary `--agent` form still applies
   alongside `--persona`.
3. **Step 3 (renamed "What the CoS does on boot (from the
   persona)")** — reconciled with step 2's persona install.
   Describes the persona's actual bootstrap: read
   `first-run-interview.md` on a fresh network; consult sibling
   personas as reference when spinning up peers; own the private
   cos repo state. Clarifies personas is READ-only reference; the
   cos repo is the writable per-user state.
4. **Step 5 (Resume recipe)** — includes `--persona` on the resume
   command; notes re-passing is safe (overwrites `PERSONA.md` with
   same bytes; the `@PERSONA.md` line in `CLAUDE.md` is
   idempotent).
5. **Troubleshooting** — adds two entries: "CoS boots as generic
   Claude" (probably launched without `--persona`; verify PERSONA.md
   + CLAUDE.md wiring) and "`--persona` path didn't resolve"
   (verify the personas clone in step 1).

Verified against the actual code: `st launch --persona <path>`
copies to `<cwd>/PERSONA.md`, creates or edits `CLAUDE.md` with a
`@PERSONA.md` import, and git-excludes both. Verified against
https://github.com/myobie/personas that `chief-of-staff.md` exists
at the repo root and instructs the fresh agent to run
`first-run-interview.md` before anything else.

### Fixed (`st launch` pins `ST_ROOT` into the generated pty.toml when the invoker set it)

Historic behavior: the generated pty.toml's `[sessions.*.env]`
carried only `ST_AGENT`. Isolation of an st-launched agent to a
non-default state root (`ST_ROOT=/tmp/eval-scratch/state st launch
…`) worked on first boot via env inheritance (invoker → st launch
→ pty up → session), but a later `pty up` / `pty restart` /
`pty gc` resurrection from a shell without `ST_ROOT` exported
silently fell back to the live default (`~/.local/state/smalltalk`).
Real isolation hole for anyone running an isolated tree —
eval-cell live proof: an st-launched agent's status write landed
in the isolated root the first time; a manual restart would have
landed in the live bus.

Fix: when the invoker had `ST_ROOT` (or legacy `COORD_ROOT`)
explicitly set, bake the resolved absolute path into every
`[sessions.*.env]` block as `ST_ROOT` — main session + codex ding
sidecar. Default-path launches leave the env unset so pty.toml
doesn't freeze today's default into tomorrow's restarts.

Scope kept minimal:
- Only `ST_ROOT` propagates; identity is already baked as
  `ST_AGENT`, and both hook scripts and CLI code use the resolution
  chain that prefers ST_AGENT → ST_IDENTITY → COORD_IDENTITY, so
  the legacy aliases don't need explicit baking.
- No `--env KEY=VAL` passthrough (deferred). Auto-propagate covers
  the flagged isolation case; a generic passthrough is a small
  follow-up if the need surfaces.
- 4 new unit tests cover: no-invoker-env (nothing baked), ST_ROOT
  set (baked), legacy COORD_ROOT set (canonicalized to ST_ROOT),
  codex ding sidecar (both env blocks pinned).

Unblocks eval cells retiring their bespoke launchers for real
`st launch`.

### Fixed (channel-watcher re-delivers inbox contents on start — at-least-once)

Historic behavior: on `startChannelWatcher`, an initial
`readdirSync(inboxDir)` seeded every existing filename into the
`seen` dedup set — a suppression policy that assumed the boot
ritual's `coord_msg_ls` would recover any backlog. Live repro
during the P5 team-standup re-run showed the policy is too
aggressive: when Claude Code reconnects the MCP stdio transport
mid-session (the current server process exits on `mcp.server.
onclose`, Claude Code respawns a fresh one), any messages that
landed in the inbox during the down window get seeded into the
new process's `seen` and are never emitted as channel
notifications. The polling backstop (brief-020 HB-4) can't rescue
because it dedups against the same `seen` set, and asyncRewake
doesn't force a turn when the agent's actively working — so a live
CoS session watched 5 messages arrive in inbox with zero
`<channel>` blocks injected. Cos hit this live.

Now the initial scan **enqueues** those files instead of seeding
them. Each fires exactly once per process, in chronological order
(`<unix-ms>-<rand6>.md` filenames lex-sort chronologically; `rand6`
breaks ties deterministically). Duplicates across a process
restart are the accepted tradeoff — per Nathan: "duplicates don't
matter to me. We want at-least-once, not at-most-once." Agent
re-reads + archives are harmless; lazy-read sweep clears the
byte-identical inbox twin on the next read.

- Test `mcp-channel-watcher.test.ts:428` (previously "files
  already present at startup DO NOT get replayed") inverts to
  "files present at startup ARE delivered on watcher start (P5R-F2
  fix)" — asserts three planted files arrive in chronological
  order and that the polling backstop tick does NOT re-fire them.
- No safety valve on backlog size. Inboxes stay small in practice
  (boot ritual drains them). If big-backlog flood bites later, cap
  is a small follow-up.

Operator-visible: on a mid-session MCP reconnect, expect to see
`<channel>` blocks for anything that arrived during the down
window — including messages you may have already read via
`coord_msg_ls`. The dup is easy to spot (same filename); archive
as normal.

### Fixed (F1 auto-poker was addressing sessions with a slash — pty resolves with a dash)

F1's `--unattended` auto-poker (#47) built its target as
`${identity}/${sessionName}` (slash), but pty joins prefix + session
name with a DASH — see `../pty/src/ptyfile.ts:58`:
`defaultDisplayName = prefix ? '${prefix}-${rawName}' : rawName`.
Every `pty send` returned `Session "<x>/<y>" not found`, no poke
fired, and every CoS-spawned worker deadlocked at the
`--dangerously-load-development-channels` gate.

Surfaced by the P5 team-standup re-run — the eval that verified F1
also caught F1's own bug. A hand-written dash-form poke
(`pty send taskflow-dev-claude`) cleared the gate instantly,
proving the fix scope.

One-line fix: switch the poker target from
`${identity}/${sessionName}` to `${identity}-${sessionName}`.
Now `st launch --unattended` gives truly 0-poke CoS-driven worker
standup. Existing F1 tests updated + a negative assertion added
(the slash form must NOT appear).

### Fixed (`st launch` backgrounds `pty up` when stdin is not a TTY)

Historic behavior: `st launch` shelled out to `pty up` with
`stdio: 'inherit'` and awaited exit. `pty up` blocks ~47s+ waiting
on its interactive attach ("ctrl+b to run in background") — an
attended operator can read that and act, but a CoS spawning a
specialist under `spawn()` misreads the block as a hang.

Now: when `stdinIsTty()` reports false (a CoS via `spawn()`,
headless CI, etc.), `st launch` spawns `pty up` detached with
`stdio: 'ignore'`, `unref`'s the child, and returns immediately.
Attended launches (real TTY) keep the current inherit-and-wait
behavior so the operator still sees pty up's UI.

Auto-detected via the same `stdinIsTty` signal F1's `--unattended`
poker uses, so hands-off standup and non-blocking pty up flip on
together for CoS-spawned specialists.

### Fixed (`st launch` pre-approves the project MCP server + renames the emitted key to `st`)

Every fresh Claude Code session prompts "Enable the `st` MCP
server?" the first time it sees a project `.mcp.json` — a hands-off
CoS spawning a specialist under `spawn()` had nobody at the REPL
to answer that. Prior generated `.claude/settings.local.json`
omitted the pre-approval fields, so the specialist wedged.

`buildClaudeSettings` now emits both docs-confirmed fields:

- `enableAllProjectMcpServers: true` — blanket-approves any server
  in the project `.mcp.json`.
- `enabledMcpjsonServers: ["st"]` — pins the specific entry so a
  future `.mcp.json` edit that adds an unrelated server still
  requires an explicit approval. Belt-and-braces alongside the
  blanket allow.

Verified in the Claude Code settings docs
(https://code.claude.com/docs/en/settings) — both fields are
load-bearing. The folder-trust dialog is handled by F1's
`--unattended` poker (same Enter). This closes F2 of the
P5-eval-surfaced unattended-standup fix set.

`st init` (and the `st launch` `.mcp.json` path that fans out
through it) also renamed the emitted `mcpServers` key from `coord`
to `st`. The on-disk `.mcp.json` state was already `st` across
swept repos, so launch was drifting: it wrote `coord:` into freshly-
generated `.mcp.json` files while every peer file was `st:`.
`enabledMcpjsonServers` above matches the new key, so a new agent
comes up with a consistent `st`-branded MCP wiring end-to-end.

### Added (`st launch --unattended` auto-pokes Claude Code's first-launch gates)

Every freshly-launched Claude Code session stalls at up to three
first-launch TUI dialogs — workspace trust, the
`--dangerously-load-development-channels` warning, and (on resume)
the resume-mode picker — each dismissed with a single Enter. Prior
to this, a CoS spawning a specialist under `spawn()` had nobody at
the REPL to press Enter, so the specialist wedged.

`st launch --unattended` bakes a startup auto-poker into the pty
session command:

```
(sleep 4 && pty send <id>/<sess> --seq key:return; sleep 4 && ...; sleep 4 && ...; sleep 4 && ...) & exec <claude ...>
```

Four pokes with 4s spacing — enough margin for a slow box, extras
past the last dialog land as empty submissions at the model prompt
(no-ops). The target is the fully-qualified pty session name
established by the identity-prefix fix (`<identity>/<sessionName>`).

**Auto-on when stdin is not a TTY** — a CoS shelling out via
`spawn()` gets hands-off standup with no flag to remember. Explicit
`--attended` is the escape hatch for headless debug runs.

Only affects the `claude` harness on the pty path. Codex has no
dev-channels gate; `--no-pty` has no session to send to. Ignored in
both cases even when `--unattended` is explicit, so callers can
pass the flag generically.

Lands `LaunchResult.unattended: boolean` for observability + a new
`unattended:     yes|no` line in the `--dry-run` summary.

### Fixed (`st launch` derives the pty prefix from the identity, not the repo basename)

Historic behavior: the generated `pty.toml` used the cwd basename as
the pty session prefix, so an agent with identity `taskflow-dev`
launched from `taskflow/` produced pty sessions namespaced under
`taskflow/…`. That drifted the pty session away from the identity —
a generic shepherd or `pty send` targeting the identity couldn't
back it up — and left two clones of the same repo (running under
different identities) racing for the same `<basename>/claude`
session key in pty's global namespace.

Now the prefix is the identity itself. `st launch --identity
taskflow-dev` under any cwd produces `pty.toml` with `prefix =
"taskflow-dev"`, so the full pty session is
`taskflow-dev/claude` — matches the identity, unique by
construction, uncollidable across clones.

`resolveRepoPrefix(cwd)` (the old derivation) was the only caller of
that helper, so it's gone too. `--session-name <name>` still
overrides the session key within the identity namespace when a
user wants a different shape.

### Docs (onboarding.md: fill in the `bin/st-evals readiness` invocation)

st-evals published at https://github.com/myobie/st-evals — a
capability-gated hermetic smoke suite. Onboarding step 4's
readiness placeholder now names the real invocation
(`bin/st-evals readiness`, run from the cloned st-evals repo root),
plus the two auxiliary probes (`preflight`, `list`) and the
`PERSONAS_DIR` / `bin/ensure-personas.sh` hook for offline mirroring.

### Docs (onboarding.md rewrite — leads with the CoS quickstart)

Full rewrite of `notes/onboarding.md`. Previously walked a
7-step manual bus-provisioning recipe (identity, folder, message
round-trip, MCP init, sync) with no mention of `st launch`. Now
leads with the Chief-of-Staff quickstart:

1. Install smalltalk + pty side-by-side; `npm install && npm link`;
   note the Claude Code hook install path.
2. `st launch claude --identity cos` in a directory that will
   become your private cos repo. `--agent <name>` for aliased
   claude binaries (Johannes-style `cl1`, `cl2`).
3. The CoS itself consumes https://github.com/myobie/personas
   (SHA-pinned) for its role contract on first boot.
4. First run: the CoS runs an interview (identity/repos/priorities/
   team/channels → writes the private cos repo) then a readiness
   check via `st-evals`. The exact `st-evals` invocation is
   deliberately not hardcoded (st-evals is still being built).
5. Operating — talk to your CoS.

The zero-to-first-message bus basics (identity, folder, message
round-trip, `st init`, multi-machine sync) live as a lower-level
"Bus basics" appendix for tooling authors and non-CoS agents.

Updated naming block, prerequisites, and troubleshooting to match
the post-#41/#42/#43 state — `st` canonical, `st --version`
available, `.claude/settings.local.json` commands bake `ST_BIN`.

### Changed (hooks resolve the CLI via `$ST_BIN` — absolute path baked at launch)

The three shipped Claude Code hooks (`pre-compact.sh`, `stop-failure.sh`;
`session-start.sh` doesn't shell out) previously relied on PATH lookup
of `coord` at hook-execution time. That's fragile: if PATH is degenerate
in Claude Code's hook-exec env, or if a stale `coord` from a different
install is on PATH, the hook silently uses the wrong binary — or none.

Now:

- **Hook scripts** resolve the CLI via `${ST_BIN:-$(command -v st ||
  command -v coord)}`. Prefers an absolute path injected by `st
  launch`; falls back to PATH lookup (st-first, coord for back-compat)
  for hand-wired settings.local.json files.
- **`st launch`** bakes the absolute path via
  `resolveStBinPath(resolveCoordBinPath())` and injects it as
  `ST_BIN=<abs>` in each hook's `command:` string. Claude Code runs
  shell-form commands via `sh -c`, so the POSIX assignment-preceded
  simple command form applies. Verified against
  https://code.claude.com/docs/en/hooks — the settings.json schema
  has no per-hook `env:` field, but shell-form command strings are
  the intended surface for this.
- New `LaunchInput.stBinForHooks?: string | null | undefined` test
  seam parallel to `hooksDir`: pass `null` to skip the injection
  (bare script paths — proves the hook's PATH-fallback branch); pass
  a synthetic string to pin a deterministic snapshot; omit for the
  auto-resolution behavior.

Anti-impersonation posture unchanged. No new spawn surface — the
hook already spawns `coord`, we just bake the exact binary.

### Added (`st --version` prints the CLI version)

`<name> --version` prints `<invokedName> <semver>\n` (e.g. `st 0.3.0`)
and exits 0. Follows the same brand-per-name convention as the help
banners — `coord --version` prints `coord 0.3.0`, `smalltalk --version`
prints `smalltalk 0.3.0`. The semver is read from `package.json` at
runtime, so it stays in sync without a build-time constant.

- New `versionString(env)` helper in `src/cli.ts` resolves
  `package.json` via `fileURLToPath(import.meta.url)` so it works
  under `npm link` (where the source lives elsewhere on disk).
- Top-level help banner now names both flags: `usage: <name> --help
  | --version`.
- Only `--version` is honored — no `-v` (traditionally means verbose),
  no bare `version` subcommand (would shadow any user's `st-version`
  plugin).

### Changed (help banners + error prefixes reflect the invoked name)

Every `usage: …` banner and every top-level `<name>: <message>` error
prefix now uses the name the user typed (`st`, `smalltalk`, or the
back-compat `coord`), sourced from the `_ST_INVOKED_AS` env var that
the `bin/` shims already export. Previously every banner said
`usage: coord …`, which read as un-renamed even when the user had
typed `st`.

- New helper `invokedName(env)` in `src/cli-context.ts` reads
  `env._ST_INVOKED_AS`, defaulting to `st` when unset (fresh dev
  shells, unit tests, direct `node src/cli.ts` invocations).
- Threaded through the top-level `runCli` banners, the message-group
  banner, every subcommand's `--help` string, and the two top-level
  error prefixes (`unknown subcommand: …`, generic `<name>: <msg>`).
- No shim or hook changes needed — `bin/st`, `bin/smalltalk`, and
  `bin/coord` already set `_ST_INVOKED_AS` from `basename $0` before
  exec'ing into node.

Users who still type `coord …` see `usage: coord …` (their existing
behavior). Users who type `st …` see `usage: st …` — closing the
"still says coord" gap that made the rename feel unfinished.

### Fixed (`st status <agent> --set <state>` lazy-creates the agent folder)

The onboarding walkthrough tells newcomers to run
`st status <self> --set available` as their first command. Pre-fix,
this threw `agent folder missing for <self>` because the explicit
`<agent>` positional went through the strict folder-existence check.
Now the `--set` path lazy-creates `<agent>/{inbox,archive}` if
missing, matching what the docs already promised.

- Adds a new `'lazy-create'` policy to `resolveAgent` alongside the
  existing `'lenient'`. When set, an explicit-identity call creates
  the folder inline instead of failing.
- `cmdStatus` uses the new policy iff `setState !== undefined` —
  passive `st status <peer>` (get) still requires the folder to
  exist so `st status ghost` doesn't silently materialize a phantom.
- Environment-resolved calls (`ST_AGENT=alice st status --set …`)
  already lazy-created via the implicit-bootstrap path; no change.

Anti-impersonation posture: the `--set` write goes to
`<agent>/status`, not another agent's inbox. Any local caller could
already `mkdir $ST_ROOT/<other>/{inbox,archive}` + `echo state > …`
by hand — the convenience of first-command bootstrap outweighs the
marginal loss.

### Added (`st launch --agent <name>` / `$AGENT` env — aliased claude binaries)

`st launch claude` now invokes a configurable binary as the harness
executable, so hosts running aliased builds (`cl1`, `cl2`,
`claude-preview`, etc.) don't have to symlink `claude`. Resolution
precedence: `--agent <name>` flag > `$AGENT` env > `'claude'` default.

- The alias is threaded through BOTH the bootstrap argv
  (`<agent> --print --session-id …`) and the main argv
  (`<agent> --resume …`), and baked into the generated `pty.toml`
  `command = "…"` line so the pty-spawned process invokes the alias
  too. No callers see a byte-level change when neither flag nor env
  is set — the `'claude'` default is preserved.
- The pty `session-name` stays independent — it defaults to the
  harness kind (`claude`) so the pty layout stays consistent even
  when the underlying binary is `cl1`. Override via
  `--session-name <name>` if you want a different session key.
- Codex launches ignore this (codex has its own launcher). The
  resolution still populates the dry-run summary for consistency,
  but the codex argv is untouched.

Only affects the `claude` harness. Empty `--agent ""` falls through
to `$AGENT`, then to the default (matching how `--permission-mode ""`
already behaves).

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
