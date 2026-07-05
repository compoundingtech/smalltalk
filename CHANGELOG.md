# Changelog

All notable changes to `@myobie/coord` (renaming → `@myobie/smalltalk`) are
recorded here. The project is pre-1.0; expect breaking changes in
minor releases until 1.0.

## Unreleased

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
