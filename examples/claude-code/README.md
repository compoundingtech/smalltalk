# Claude Code integration — smalltalk session-boundary hooks

This directory has the pieces for plugging smalltalk into Claude Code
at the **session boundary** layer: a `SessionStart` hook that runs the
boot ritual and rehydrates durable working-state on every cold start /
resume / `/clear` / `/compact`, a `PreCompact` hook that stubs
`context/now.md` right before compaction so boot-rehydrate has
something to inject, and a `StopFailure` hook that surfaces API-error
wedges to myobie via smalltalk so a quiet, wedged session doesn't go
unnoticed.

## What's in here

- `hooks/session-start.sh` — Claude Code `SessionStart` hook.
  Injects the agent's `context/now.md` as a `<context>` block (when
  present + fresh) then wakes the agent with a system reminder to run
  the smalltalk boot ritual (status → available, drain inbox).
  Absent-able: skips the injection cleanly when `context/now.md` is
  missing or has aged past the staleness threshold (24h default). See
  **brief-024 hook-legs** below.
- `hooks/pre-compact.sh` — Claude Code `PreCompact` hook. Fires just
  before compaction wipes the in-context state. Writes a stub to
  `context/now.md` if the model hasn't flushed a fresh one recently, so
  the next boot-rehydrate has something to inject. Exit 0 always;
  errors go to a log file, never stderr. See **brief-024 hook-legs**
  below.
- `hooks/stop-failure.sh` — Claude Code `StopFailure` hook. Fires when
  a session ends mid-turn due to an Anthropic API error; branches by
  `error_type` and either sets the agent's status (away/offline) and
  optionally `st message send`s myobie a tuned notice — or stays
  silent for programmer-error types. See **StopFailure** below.
- `settings.local.example.json` — full example settings file with all
  three hooks wired up. Copy to `<agent-repo>/.claude/settings.local.json`
  and swap the absolute paths for your smalltalk checkout.

## brief-024 hook-legs (SessionStart rehydrate + PreCompact flush)

Together with the `coord context read/write/append` verbs and the
`~/.local/state/coord/<agent>/context/` folder that brief-024 v1
shipped, these two hooks close the **lossless-restart** loop for the
in-context-state leg:

1. **During a session**, the model writes fresh state to
   `context/now.md` at each meaningful step (base-persona rule 2: flush
   as you go). It also appends decisions with a `why` to
   `context/decisions.md` (append-only log).

2. **Just before compaction**, `pre-compact.sh` fires. If the model
   flushed recently (default: within 5 min, tunable via
   `$COORD_PRECOMPACT_FRESH_S`), it leaves `now.md` alone. Otherwise
   it writes a "compaction fired without a recent flush" stub via
   `coord context write` — atomic tmp+rename, so a concurrent read
   never sees a partial file. **exit 0 always**; errors go to
   `context/.flush-errors.log`, never stderr, because a hook that
   writes to stderr on the `PreCompact` boundary would inject noise
   into every post-compaction turn.

3. **On session start** (fresh, `--resume`, `/clear`, `/compact`),
   `session-start.sh` reads `context/now.md`. If present and
   *fresh* (default staleness: 24h, tunable via
   `$COORD_REHYDRATE_STALE_S`), it injects the file as a `<context
   source="coord/context/now.md" agent="…">…</context>` block into the
   agent's stderr — Claude Code surfaces stderr as a system reminder
   under `asyncRewake: true`, so the injected block lands as the first
   thing the model sees. Then the boot-ritual reminder follows. When
   `context/now.md` is absent or stale, the hook falls through to the
   pre-brief-024 behavior — just the ritual reminder, no injection.

**Absent-able is load-bearing.** The `context/` folder does not need to
exist for either hook to work; both fall through cleanly and produce
identical behavior to pre-brief-024 wiring. This is what lets
evals-claude's restart-continuity eval A/B a control arm (no context/)
against a treatment arm (populated context/) without special-casing.

**The PreCompact hook is a backstop, not the primary flush mechanism.**
Model discipline (flush proactively at each meaningful state change) is
the real lever. The hook exists so a model that fails to flush before
compaction still leaves a machine-readable trace, not so writing
`context/` becomes optional.

## Why it's needed

Channel mode (`st mcp --channel`) handles **new arrivals during a
session**: the chokidar watcher emits `notifications/claude/channel`
frames and the SDK surfaces them as `<channel source="coord" …>`
blocks in the agent's context, triggering a turn. (The `source="coord"`
attribute is kept through the rename window for back-compat with
downstream parsers; the Phase 5 cleanup flips it to `source="st"`.)

What it doesn't handle is **start-of-session**. When Claude Code boots
fresh, or resumes a saved conversation (`--resume`, `--continue`,
`/resume`, `/clear`, `/compact`), the MCP server reconnects and the
ritual instructions are loaded — but if nothing new lands in the
inbox, the agent never gets a turn, so the ritual never runs. Result:
the agent stays silent with stale status and an unread inbox.

This hook closes that gap. The `SessionStart` hook fires on every
session boundary listed above; with `asyncRewake: true` the script
runs in the background and Claude Code injects its stderr as a system
reminder, which counts as a turn-triggering event. The agent reads
the reminder, runs the ritual, and the session is in a known-good
state from the first turn.

## Install

1. Drop the hook somewhere stable. The default if you cloned this
   repo is `<this-repo>/examples/claude-code/hooks/session-start.sh`;
   copy or symlink it wherever you prefer.

2. Add both hooks to either `~/.claude/settings.json` (user-scope,
   every Claude Code session on the machine) or
   `<repo>/.claude/settings.local.json` (project-scope, just one repo).
   `settings.local.example.json` in this directory is the full
   reference shape; the short form is:

   ```json
   {
     "hooks": {
       "SessionStart": [
         {
           "hooks": [
             {
               "type": "command",
               "async": true,
               "asyncRewake": true,
               "command": "/absolute/path/to/smalltalk/examples/claude-code/hooks/session-start.sh"
             }
           ]
         }
       ],
       "StopFailure": [
         {
           "hooks": [
             {
               "type": "command",
               "command": "/absolute/path/to/smalltalk/examples/claude-code/hooks/stop-failure.sh"
             }
           ]
         }
       ]
     }
   }
   ```

   Use absolute paths — Claude Code does not resolve `~` or
   repo-relative paths in hook commands. The `StopFailure` hook
   doesn't need `asyncRewake` (it doesn't need to inject a turn into
   the wedged session — it just records what happened).

3. (If you're using channel mode for push, which you probably are:)
   make sure Claude Code is launched with
   `--dangerously-load-development-channels server:coord` so the
   `<channel source="coord">` blocks surface as visible turns. The
   `server:coord` token is the MCP server's announced name when
   invoked via `bin/coord`; Phase 5 of the rename flips it to
   `server:st`. `experimental.channelsEnabled: true` in Claude Code's
   settings is the durable form of the same opt-in.

## How it relates to channel mode

The two are **complementary**, not redundant.

| When | What fires | What runs |
|---|---|---|
| Session boundary (cold start, resume, /clear, /compact) | `SessionStart` hook → system reminder | Boot ritual (status set, inbox drain) |
| Mid-session new inbox file | `notifications/claude/channel` MCP push | Read + reply + archive the specific message |

If you only run the hook, channel pushes still work — but you'd miss
real-time pings during the session. If you only run channel mode,
pushes work but resumed sessions stay silent until something new
arrives. Both together is the intended setup.

## StopFailure — surfacing API-error wedges

Claude Code emits a `StopFailure` hook event whenever a session ends a
turn because of an API error (rate-limit, auth failure, billing,
transient 5xx, etc.). When that happens, the agent stops responding
and there's no obvious signal to a human that anything's wrong — the
session just goes quiet. Multiply by N agents on the machine and you
get the "all-quiet-Anthropic-capacity-wedge" failure: nobody notices
for hours.

`hooks/stop-failure.sh` closes that gap **without** auto-restarting —
myobie's explicit call is "just visibility, not automation." The hook
reads the JSON envelope Claude Code pipes on stdin, parses
`error_type` via jq, and applies a tuned policy:

| `error_type`                                  | What the hook does                                                                                                          |
| --------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------|
| `rate_limit`                                  | `st status <id> --set away`. **No** ding (transient — would spam during Anthropic capacity events).                         |
| `server_error`                                | `st status <id> --set away` + standard-priority `st message send myobie` notice.                                            |
| `authentication_failed`                       | `st status <id> --set offline` + **high-priority** `st message send myobie` (needs human intervention).                     |
| `oauth_org_not_allowed`                       | Same as `authentication_failed`.                                                                                            |
| `billing_error`                               | `st status <id> --set offline` + high-priority "billing issue" notice.                                                      |
| `max_output_tokens`, `invalid_request`, `model_not_found` | **Ignored.** Programmer error / long-turn config issue — not infrastructure; the agent owns recovery.           |
| anything else (incl. literal `unknown`)       | `st status <id> --set away` + standard-priority notice with the `error_type` verbatim, so we extend the table after triage. |

The script reads the agent's identity from `$ST_AGENT` (or the legacy
`$ST_IDENTITY` / `$COORD_IDENTITY` fallbacks), which should be set in
the session env (e.g. for a pty-supervised Claude Code session, set
it in `pty.toml`'s `[sessions.claude.env]` block).

**Why a single script with internal branching**, rather than one
matcher-entry per `error_type` in `settings.local.json`? The policy is
likely to evolve as we see new wedges in the wild; keeping the
branching in the script means tuning the table is a one-file edit,
not a settings.json migration across every agent's repo.

**Not in scope here**: wedge detection cron, auto-restart, or any
auto-recovery. This is visibility only. A human sees the ding, looks
at `st agents`, and decides whether to nudge or wait.

## How it pairs with the other harnesses

- **Pi** has an analogous `pi.sendUserMessage` call inside
  `examples/pi/coord.ts`'s `session_start` handler. Same idea: force
  a turn on every session boundary so the boot ritual runs. (The
  extension filename `coord.ts` is preserved for back-compat with
  installed pi extensions; new installs can use any filename.)
- **Codex** doesn't have an asyncRewake equivalent. The existing
  `examples/codex/session-start.sh` hook already runs on each new
  session and injects the inbox snapshot as `additionalContext`,
  which serves the same nudge purpose for that harness.

## Boundaries

- The hook **doesn't shell out to `st`** itself. The boot ritual
  is documented in the MCP server's `CHANNEL_INSTRUCTIONS` string and
  the smalltalk SKILL.md — by the time the agent reads the system
  reminder, those instructions are already loaded. The hook just
  triggers a turn.
- The hook **doesn't auto-set status** or auto-drain the inbox. That's
  the agent's job, per the ritual. Doing it from the hook would
  collide with the agent's own actions and break the
  one-writer-per-agent rule.
- This script is **per-machine, per-user setup**. It's not bundled
  into anyone's automatic install path — you copy/symlink + edit
  settings.json yourself.
